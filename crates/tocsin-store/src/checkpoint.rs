// SPDX-License-Identifier: AGPL-3.0-only

//! The checkpointer: copies the write-ahead log back into the database file
//! on its own connection and thread, so a checkpoint's fsyncs never run
//! inside a write.
//!
//! SQLite's automatic checkpoint runs inside whichever commit pushes the log
//! past 1000 pages, on the committing connection. Here that is the writer
//! thread, so every append queued behind that commit waited for the copy and
//! both fsyncs (20 to 480 ms on CI runners, TOC-57). The writer connection
//! has automatic checkpoints turned off; its WAL hook counts the frames each
//! commit appends, and it nudges this thread after each write. This thread
//! runs a `PASSIVE` checkpoint, which takes no lock the writer or the readers
//! need, once [`THRESHOLD_FRAMES`] frames are waiting, or [`MAX_DELAY`] after
//! the first of fewer.
//!
//! A checkpoint racing the writer copies only the frames committed before it
//! started. SQLite rewinds the log to its start only when a write finds every
//! frame copied, so under writes that never pause the log would grow without
//! bound. Once it holds [`LOG_LIMIT_FRAMES`], the writer checkpoints the rest
//! itself: no commit can race that one, and its next commit rewinds the log.
//!
//! Only one connection can checkpoint at a time, and a `PASSIVE` checkpoint
//! that finds another one running copies nothing. Writes go on during this
//! thread's passes, which can take seconds on a disk still flushing other
//! work (TOC-59), so a writer that carried on past the limit would grow the
//! log by as much as it can write in that time. It waits for the other
//! checkpoint instead, for up to [`CHECKPOINT_WAIT`], and commits nothing
//! meanwhile.

use std::cell::Cell;
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use rusqlite::hooks::Wal;

use crate::error::Result;

/// Uncopied frames that start a checkpoint: SQLite's own automatic threshold,
/// about 4 MB at the default page size.
const THRESHOLD_FRAMES: u64 = 1000;

/// Longest fewer than [`THRESHOLD_FRAMES`] frames wait to be copied, so a
/// quiet store still checkpoints.
const MAX_DELAY: Duration = Duration::from_secs(60);

/// How soon a checkpoint that readers held back is tried again.
const RETRY: Duration = Duration::from_secs(1);

/// Checkpoints in a row that leave a log of [`THRESHOLD_FRAMES`] or more
/// behind before a warning.
const STALLED_WARNING: u32 = 10;

/// Frames in the log at which the writer checkpoints the rest itself.
pub(crate) const LOG_LIMIT_FRAMES: c_int = 4000;

/// Longest the writer waits at [`LOG_LIMIT_FRAMES`] for another connection's
/// checkpoint to finish: the `busy_timeout` every connection uses, several
/// times the longest pass measured on CI runners (794 ms, TOC-57).
const CHECKPOINT_WAIT: Duration = Duration::from_secs(5);

/// How often the waiting writer tries its checkpoint again. A try that finds
/// the lock taken costs tens of microseconds.
const CHECKPOINT_POLL: Duration = Duration::from_millis(1);

/// The log file's size once a rewind has emptied it: about
/// [`LOG_LIMIT_FRAMES`] frames of 4 KiB pages. A log that a long read kept
/// from rewinding is cut back to this, not left at its largest.
pub(crate) const LOG_LIMIT_BYTES: i64 = 16 * 1024 * 1024;

/// SQLite's default `wal_autocheckpoint`, restored on the writer if this
/// thread is ever gone.
const AUTOCHECKPOINT_PAGES: i64 = 1000;

thread_local! {
    /// The log's length in frames after the last commit on this thread, and
    /// the frames all its commits appended. Only the writer thread commits
    /// on a watched connection.
    static LOG: Cell<(c_int, u64)> = const { Cell::new((0, 0)) };
}

/// Counts the frames each commit on `conn` appends, for [`Nudge`].
pub(crate) fn watch(conn: &Connection) {
    conn.wal_hook(Some(on_commit));
}

/// SQLite's WAL hook: `frames` is the log's length after the commit.
#[allow(clippy::unnecessary_wraps)] // the hook's signature
fn on_commit(_: &Wal, frames: c_int) -> rusqlite::Result<()> {
    let (before, appended) = LOG.get();
    // A shorter log has been rewound to its start since the last commit.
    let added = if frames >= before {
        frames - before
    } else {
        frames
    };
    LOG.set((frames, appended + u64::from(added.unsigned_abs())));
    Ok(())
}

pub(crate) struct Checkpointer {
    thread: JoinHandle<()>,
}

/// The writer's end: tells the checkpointer a write committed, and
/// checkpoints on the writer once the log reaches [`LOG_LIMIT_FRAMES`].
pub(crate) struct Nudge {
    link: Option<Link>,
    /// The log length that next logs an error; doubles each time.
    alarm: c_int,
}

struct Link {
    sender: SyncSender<()>,
    /// Frames appended to the log, ever.
    appended: Arc<AtomicU64>,
    /// The checkpointer is partway through a pass. Only reported: the writer
    /// waits on SQLite's own lock, which any connection can hold.
    checkpointing: Arc<AtomicBool>,
}

impl Checkpointer {
    /// Starts the checkpointer on `conn`. It stops once the returned
    /// [`Nudge`] is dropped, which happens when the writer thread ends.
    pub(crate) fn spawn(conn: Connection) -> Result<(Checkpointer, Nudge)> {
        // One pending nudge is enough: it means "the log has grown".
        let (sender, nudges) = mpsc::sync_channel(1);
        let appended = Arc::new(AtomicU64::new(0));
        let checkpointing = Arc::new(AtomicBool::new(false));
        let thread = {
            let appended = Arc::clone(&appended);
            let checkpointing = Arc::clone(&checkpointing);
            std::thread::Builder::new()
                .name("tocsin-store-checkpointer".into())
                .spawn(move || run(&conn, &nudges, &appended, &checkpointing))?
        };
        Ok((
            Checkpointer { thread },
            Nudge::new(sender, appended, checkpointing),
        ))
    }

    /// Waits for the thread, which closes its connection on the way out.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }
}

#[cfg(test)]
impl Checkpointer {
    /// A checkpointer whose thread has panicked.
    pub(crate) fn panicked() -> Checkpointer {
        let thread = std::thread::spawn(|| panic!("a checkpointer bug"));
        while !thread.is_finished() {
            std::thread::yield_now();
        }
        Checkpointer { thread }
    }
}

impl Nudge {
    fn new(
        sender: SyncSender<()>,
        appended: Arc<AtomicU64>,
        checkpointing: Arc<AtomicBool>,
    ) -> Nudge {
        Nudge {
            link: Some(Link {
                sender,
                appended,
                checkpointing,
            }),
            alarm: 2 * LOG_LIMIT_FRAMES,
        }
    }

    /// Called by the writer after each write. Waits only once the log has
    /// reached [`LOG_LIMIT_FRAMES`] and another checkpoint is running.
    pub(crate) fn after_write(&mut self, conn: &Connection) {
        let Some(link) = &self.link else {
            return;
        };
        let (frames, appended) = LOG.get();
        link.appended.store(appended, Ordering::Relaxed);
        // Full means a nudge is already pending, which is all one says.
        if let Err(TrySendError::Disconnected(())) = link.sender.try_send(()) {
            // The thread can only end early by panicking. Without it the log
            // would grow without bound, so writes checkpoint it themselves
            // again, as SQLite does by default.
            self.link = None;
            let restored = conn
                .pragma_update(None, "wal_autocheckpoint", AUTOCHECKPOINT_PAGES)
                .is_ok();
            tracing::error!(
                restored,
                "the checkpointer thread is gone; writes checkpoint the log themselves again"
            );
            return;
        }
        if frames < LOG_LIMIT_FRAMES {
            self.alarm = 2 * LOG_LIMIT_FRAMES;
            return;
        }
        // The checkpointer's passes keep being raced by commits, or a read
        // holds the log. Copy what is left here, where nothing can commit
        // meanwhile, so the next commit can rewind the log. If another
        // checkpoint holds the lock, wait for it rather than append past
        // the limit for as long as it runs.
        let started = Instant::now();
        let mut outcome = passive(conn);
        let checkpointer_in_flight = link.checkpointing.load(Ordering::Relaxed);
        while locked_out(&outcome) && started.elapsed() < CHECKPOINT_WAIT {
            std::thread::sleep(CHECKPOINT_POLL);
            outcome = passive(conn);
        }
        let took = started.elapsed();
        tracing::debug!(
            frames,
            ?outcome,
            checkpointer_in_flight,
            ?took,
            "the writer checkpointed a log at its limit"
        );
        if locked_out(&outcome) {
            tracing::error!(
                frames,
                checkpointer_in_flight,
                ?took,
                "another checkpoint held the write-ahead log for longer than {CHECKPOINT_WAIT:?}; \
                 the next write goes past the log's limit"
            );
        }
        if frames >= self.alarm {
            self.alarm = self.alarm.saturating_mul(2);
            if locked_out(&outcome) {
                tracing::error!(
                    frames,
                    ?outcome,
                    "the write-ahead log is past twice its limit and still growing; \
                     another checkpoint keeps holding it"
                );
            } else {
                tracing::error!(
                    frames,
                    ?outcome,
                    "the write-ahead log is past twice its limit and still growing; a long \
                     read may be keeping it from being copied or rewound"
                );
            }
        }
    }
}

/// Another connection's checkpoint held the lock, so `outcome`'s copied
/// nothing.
fn locked_out(outcome: &rusqlite::Result<Outcome>) -> bool {
    matches!(outcome, Ok(outcome) if outcome.busy)
}

/// Frames the commits on this thread have appended to the log, ever.
#[cfg(test)]
pub(crate) fn appended() -> u64 {
    LOG.get().1
}

/// The log's length in frames after the last commit on this thread.
#[cfg(test)]
pub(crate) fn log_frames() -> u64 {
    u64::from(LOG.get().0.unsigned_abs())
}

#[cfg(test)]
impl Nudge {
    /// A nudge with no checkpointer behind it; the test holds the other end.
    pub(crate) fn detached() -> (Nudge, Receiver<()>) {
        let (sender, nudges) = mpsc::sync_channel(1);
        (Nudge::new(sender, Arc::default(), Arc::default()), nudges)
    }
}

fn run(conn: &Connection, nudges: &Receiver<()>, appended: &AtomicU64, checkpointing: &AtomicBool) {
    // The appended count the last checkpoint started from: frames past it
    // have not been copied.
    let mut copied_to = 0;
    // When the next checkpoint must run even below the threshold.
    let mut due: Option<Instant> = None;
    let mut stalled = 0;
    loop {
        let woken = match due {
            None => nudges.recv().is_ok(),
            Some(at) => !matches!(
                nudges.recv_timeout(at.saturating_duration_since(Instant::now())),
                Err(RecvTimeoutError::Disconnected)
            ),
        };
        if !woken {
            return;
        }
        let now = Instant::now();
        let target = appended.load(Ordering::Relaxed);
        let due_at = *due.get_or_insert(now + MAX_DELAY);
        if target - copied_to < THRESHOLD_FRAMES && now < due_at {
            continue;
        }
        copied_to = target;
        checkpointing.store(true, Ordering::Relaxed);
        let complete = checkpoint(conn, &mut stalled);
        checkpointing.store(false, Ordering::Relaxed);
        due = if !complete {
            Some(Instant::now() + RETRY)
        } else if appended.load(Ordering::Relaxed) > copied_to {
            Some(Instant::now() + MAX_DELAY)
        } else {
            None
        };
    }
}

/// What one `PASSIVE` checkpoint did.
#[derive(Debug)]
struct Outcome {
    /// Another connection was checkpointing, so this one did nothing. A
    /// reader never makes a `PASSIVE` checkpoint busy, only shorter.
    busy: bool,
    /// Frames in the log when it started; -1 if busy.
    log: i64,
    /// Frames of the log in the database file when it finished.
    copied: i64,
}

impl Outcome {
    fn complete(&self) -> bool {
        !self.busy && self.log == self.copied
    }
}

/// Copies as much of the log as no reader still needs. `PASSIVE` never
/// waits: frames a reader's snapshot still uses are left for later, and
/// another checkpoint in progress skips this one (see [`locked_out`]).
fn passive(conn: &Connection) -> rusqlite::Result<Outcome> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok(Outcome {
            busy: row.get(0)?,
            log: row.get(1)?,
            copied: row.get(2)?,
        })
    })
}

/// Runs a checkpoint; true if it copied the whole log. Warns once when
/// [`STALLED_WARNING`] in a row leave a large log behind.
fn checkpoint(conn: &Connection, stalled: &mut u32) -> bool {
    match passive(conn) {
        Ok(outcome) if outcome.complete() => {
            *stalled = 0;
            tracing::trace!(?outcome, "checkpointed the write-ahead log");
            true
        }
        Ok(outcome) => {
            #[allow(clippy::cast_possible_wrap)]
            let large = outcome.log >= THRESHOLD_FRAMES as i64;
            *stalled = if large { *stalled + 1 } else { 0 };
            tracing::debug!(?outcome, "a checkpoint left part of the log");
            if *stalled == STALLED_WARNING {
                tracing::warn!(
                    ?outcome,
                    "{STALLED_WARNING} checkpoints in a row could not copy the whole \
                     write-ahead log; a long read may be holding it"
                );
            }
            false
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "a write-ahead log checkpoint failed; the log grows until one succeeds"
            );
            false
        }
    }
}
