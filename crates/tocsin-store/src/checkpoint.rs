// SPDX-License-Identifier: AGPL-3.0-only

//! The checkpointer: copies the write-ahead log back into the database file
//! on its own connection and thread, so a checkpoint's fsyncs never run
//! inside a write. The two still share a disk, so a write can stall while a
//! pass runs; `docs/storage.md` has the evidence and the hypothesis.
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
//! bound. Once it holds [`SOFT_LIMIT_FRAMES`], each write wakes this thread
//! at once, and a pass that copied all it found is followed straight away by
//! one for the frames committed during it: a pass that no commit races
//! leaves the whole log copied, and the next commit rewinds it. The writer
//! itself does no checkpoint work below [`HARD_LIMIT_FRAMES`]: a checkpoint
//! on the writer thread, however few frames it copies, fsyncs twice inside
//! the write (TOC-60).
//!
//! Writes that never pause can race every pass, so at [`HARD_LIMIT_FRAMES`]
//! the writer checkpoints the rest itself: no commit can race that one, and
//! its next commit rewinds the log. Only one connection can checkpoint at a
//! time, and a `PASSIVE` checkpoint that finds another one running copies
//! nothing. Writes go on during this thread's passes, which can take seconds
//! on a disk still flushing other work (TOC-59), so a writer that carried on
//! past the hard limit would grow the log by as much as it can write in that
//! time. It waits for the other checkpoint instead, for up to
//! [`CHECKPOINT_WAIT`], and commits nothing meanwhile. A wait that times out,
//! or a checkpoint that fails, starts a lockout: one error is logged, and
//! writes past the hard limit try once without waiting until a checkpoint
//! succeeds again. While the writer checkpoints at the hard limit, this
//! thread starts no pass of its own, which would only take the lock from it.
//!
//! Retention's purge is the one writer that can outrun the checkpointer: a
//! delete chunk of 2000 rows appends hundreds of frames, chunks follow each
//! other 10 ms apart, and the passes chasing them each copy the 16 MiB
//! between the soft limit and the commits. Left alone it carries the log to
//! the hard limit, and the check result that lands there waits out an inline
//! copy (TOC-62). So before each of its writes retention [`Checkpointer::make_room`]:
//! past [`SOFT_LIMIT_FRAMES`], it waits until a pass has copied the log, for
//! up to [`PACE_WAIT`], and lets the checkpointer, not a check result, pay
//! for the frames it wrote.

use std::cell::Cell;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use rusqlite::hooks::Wal;

use crate::error::Result;
use crate::schema;
use crate::stats::{CheckpointEvent, CheckpointStats, Stats};

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

/// Frames in the log past which every write wakes the checkpointer, and a
/// pass that copied all it found is followed by another straight away.
pub(crate) const SOFT_LIMIT_FRAMES: c_int = 4000;

/// Frames in the log at which the writer checkpoints the rest itself: twice
/// [`SOFT_LIMIT_FRAMES`], so the checkpointer has as many frames again to
/// get a pass in between commits before the writer has to stop for one.
pub(crate) const HARD_LIMIT_FRAMES: c_int = 2 * SOFT_LIMIT_FRAMES;

/// Longest the writer waits at [`HARD_LIMIT_FRAMES`] for another
/// connection's checkpoint to finish: the `busy_timeout` every connection
/// uses, several times the longest pass measured on CI runners (794 ms,
/// TOC-57).
const CHECKPOINT_WAIT: Duration = schema::BUSY_TIMEOUT;

/// How often the waiting writer tries its checkpoint again when the lock is
/// held by another process, whose checkpoint it cannot wait on directly.
const CHECKPOINT_POLL: Duration = Duration::from_millis(10);

/// Longest retention waits, before one of its writes, for the checkpointer
/// to copy a log past [`SOFT_LIMIT_FRAMES`]: as long as the writer would wait
/// at the hard limit. A wait that times out logs a warning and writes
/// anyway, and the hard limit still bounds the log.
pub(crate) const PACE_WAIT: Duration = schema::BUSY_TIMEOUT;

/// The log file's size once a rewind has emptied it: about
/// [`HARD_LIMIT_FRAMES`] frames of 4 KiB pages. A log that a long read kept
/// from rewinding is cut back to this, not left at its largest.
pub(crate) const LOG_LIMIT_BYTES: i64 = 32 * 1024 * 1024;

/// SQLite's default `wal_autocheckpoint`, restored on the writer if this
/// thread is ever gone.
const AUTOCHECKPOINT_PAGES: i64 = 1000;

thread_local! {
    /// The log's length in frames after the last commit on this thread, the
    /// frames all its commits appended, and the commits that left the log
    /// shorter than the one before, so had rewound it. Only the writer
    /// thread commits on a watched connection.
    static LOG: Cell<(c_int, u64, u64)> = const { Cell::new((0, 0, 0)) };
}

/// Counts the frames each commit on `conn` appends, for [`Nudge`].
pub(crate) fn watch(conn: &Connection) {
    conn.wal_hook(Some(on_commit));
}

/// SQLite's WAL hook: `frames` is the log's length after the commit.
#[allow(clippy::unnecessary_wraps)] // the hook's signature
fn on_commit(_: &Wal, frames: c_int) -> rusqlite::Result<()> {
    let (before, appended, rewinds) = LOG.get();
    // A shorter log has been rewound to its start since the last commit.
    let (added, rewinds) = if frames >= before {
        (frames - before, rewinds)
    } else {
        (frames, rewinds + 1)
    };
    LOG.set((frames, appended + u64::from(added.unsigned_abs()), rewinds));
    Ok(())
}

pub(crate) struct Checkpointer {
    thread: JoinHandle<()>,
    log: Arc<Log>,
    stats: Arc<Stats>,
    #[cfg(test)]
    passes: Arc<Passes>,
}

/// The writer's end: tells the checkpointer a write committed, and
/// checkpoints on the writer once the log reaches [`HARD_LIMIT_FRAMES`].
pub(crate) struct Nudge {
    link: Option<Link>,
    /// The log length that next logs an error; doubles each time.
    alarm: c_int,
    /// Set while the writer's checkpoints at the limit time out or fail.
    lockout: Option<Lockout>,
    stats: Arc<Stats>,
    /// When the write now running started, for [`Stats`].
    write_started: Option<Instant>,
    /// The rewinds the last write had seen.
    rewinds: u64,
}

struct Link {
    sender: SyncSender<()>,
    log: Arc<Log>,
    passes: Arc<Passes>,
}

/// The log as of the writer's last write, for the checkpointer.
#[derive(Default)]
struct Log {
    /// Frames appended to the log, ever.
    appended: AtomicU64,
    /// Frames in the log.
    frames: AtomicU64,
    /// The appended count up to which the checkpointer's last complete pass
    /// copied the log.
    copied: AtomicU64,
}

impl Log {
    /// The log is past [`SOFT_LIMIT_FRAMES`] and holds frames no complete
    /// pass has copied, so a commit now would not rewind it.
    fn behind(&self) -> bool {
        self.frames.load(Ordering::Relaxed) >= u64::from(SOFT_LIMIT_FRAMES.unsigned_abs())
            && self.copied.load(Ordering::Relaxed) < self.appended.load(Ordering::Relaxed)
    }
}

/// The writer's checkpoints at the limit have timed out or failed since
/// `since`, and the log has held up to `peak` frames meanwhile.
struct Lockout {
    since: Instant,
    peak: c_int,
}

/// The checkpointer's passes, which the writer at the limit waits out on a
/// condition variable rather than by polling.
#[derive(Default)]
pub(crate) struct Passes {
    state: Mutex<State>,
    /// Notified when a pass ends.
    ended: Condvar,
    /// Writes at the limit that found the log locked and waited.
    #[cfg(test)]
    waits: AtomicU64,
    /// Steps of [`CHECKPOINT_POLL`] those writes slept.
    #[cfg(test)]
    polls: AtomicU64,
    /// Passes the checkpointer skipped to stand aside for the writer.
    #[cfg(test)]
    skips: AtomicU64,
    /// Passes the checkpointer ran.
    #[cfg(test)]
    ran: AtomicU64,
}

#[derive(Default)]
struct State {
    /// A pass is running.
    running: bool,
    /// No pass starts before this: the writer is checkpointing at the
    /// limit, and a pass would only take the lock from it.
    stand_aside_until: Option<Instant>,
}

/// A pass of the checkpointer's, running until this is dropped.
pub(crate) struct Pass<'a>(&'a Passes);

impl Passes {
    /// Starts a pass, unless the checkpointer is to stand aside.
    pub(crate) fn begin(&self) -> Option<Pass<'_>> {
        let mut state = self.state();
        if state
            .stand_aside_until
            .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        state.running = true;
        Some(Pass(self))
    }

    /// Keeps the checkpointer from starting a pass until `until`.
    fn stand_aside_until(&self, until: Option<Instant>) {
        self.state().stand_aside_until = until;
    }

    /// Waits until no pass is running, or until `deadline`. False if none
    /// was running, so it is another process that holds the lock.
    fn wait_out(&self, deadline: Instant) -> bool {
        let state = self.state();
        if !state.running {
            return false;
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        drop(
            self.ended
                .wait_timeout_while(state, timeout, |state| state.running)
                .unwrap_or_else(PoisonError::into_inner),
        );
        true
    }

    /// Locks the state, which a panic cannot leave inconsistent.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.state().running = false;
        self.0.ended.notify_all();
    }
}

impl Checkpointer {
    /// Starts the checkpointer on `conn`. It stops once the returned
    /// [`Nudge`] is dropped, which happens when the writer thread ends.
    pub(crate) fn spawn(conn: Connection) -> Result<(Checkpointer, Nudge)> {
        // One pending nudge is enough: it means "the log has grown".
        let (sender, nudges) = mpsc::sync_channel(1);
        let log = Arc::new(Log::default());
        let passes = Arc::new(Passes::default());
        let stats = Arc::new(Stats::default());
        let thread = {
            let log = Arc::clone(&log);
            let passes = Arc::clone(&passes);
            let stats = Arc::clone(&stats);
            std::thread::Builder::new()
                .name("tocsin-store-checkpointer".into())
                .spawn(move || run(&conn, &nudges, &log, &passes, &stats))?
        };
        Ok((
            Checkpointer {
                thread,
                log: Arc::clone(&log),
                stats: Arc::clone(&stats),
                #[cfg(test)]
                passes: Arc::clone(&passes),
            },
            Nudge::new(sender, log, passes, stats),
        ))
    }

    /// Waits for the thread, which closes its connection on the way out.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }

    /// What the writer and this thread have done to keep the log bounded.
    pub(crate) fn stats(&self) -> CheckpointStats {
        self.stats.snapshot()
    }

    /// Before a bulk write: while the log is past [`SOFT_LIMIT_FRAMES`] and
    /// no pass has copied it, waits for one, for up to [`PACE_WAIT`], so the
    /// bulk writer, not the writes queued behind it, waits for the copy. The
    /// caller holds no transaction and nothing of the writer's meanwhile, so
    /// other writes go on. A wait that times out logs a warning.
    pub(crate) async fn make_room(&self) {
        if !self.log.behind() {
            return;
        }
        let started = Instant::now();
        while self.log.behind() && started.elapsed() < PACE_WAIT {
            tokio::time::sleep(CHECKPOINT_POLL).await;
        }
        let waited = started.elapsed();
        self.stats.paced(started, waited);
        if self.log.behind() {
            tracing::warn!(
                frames = self.log.frames.load(Ordering::Relaxed),
                ?waited,
                "no checkpoint copied the write-ahead log past its soft limit in time; \
                 retention writes anyway"
            );
        }
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
        Checkpointer {
            thread,
            log: Arc::default(),
            stats: Arc::default(),
            passes: Arc::default(),
        }
    }

    /// Writes at the limit that found the log locked and waited.
    pub(crate) fn waits(&self) -> u64 {
        self.passes.waits.load(Ordering::Relaxed)
    }
}

impl Nudge {
    fn new(sender: SyncSender<()>, log: Arc<Log>, passes: Arc<Passes>, stats: Arc<Stats>) -> Nudge {
        Nudge {
            link: Some(Link {
                sender,
                log,
                passes,
            }),
            alarm: 2 * HARD_LIMIT_FRAMES,
            lockout: None,
            stats,
            write_started: None,
            rewinds: 0,
        }
    }

    /// Called by the writer as it starts a write, to time it.
    pub(crate) fn before_write(&mut self) {
        self.write_started = Some(Instant::now());
    }

    /// Called by the writer after each job, before it replies: tells the
    /// checkpointer how long the log is now, so a caller that paces on it
    /// never reads the length from before its own write.
    pub(crate) fn publish(&self) {
        let Some(link) = &self.link else {
            return;
        };
        let (frames, appended, _) = LOG.get();
        link.log.appended.store(appended, Ordering::Relaxed);
        link.log
            .frames
            .store(u64::from(frames.unsigned_abs()), Ordering::Relaxed);
    }

    /// Called by the writer after each write and its replies. Publishes the
    /// log's length again, for callers that skip [`Nudge::publish`], and
    /// checkpoints only once the log has reached [`HARD_LIMIT_FRAMES`],
    /// waiting only if another checkpoint is running then and no lockout is.
    pub(crate) fn after_write(&mut self, conn: &Connection) {
        let (frames, _, rewinds) = LOG.get();
        let rewound = rewinds > self.rewinds;
        self.rewinds = rewinds;
        let wrote = self.write_started.take();
        self.stats.write(
            wrote.unwrap_or_else(Instant::now),
            wrote.map_or(Duration::ZERO, |at| at.elapsed()),
            u64::from(frames.unsigned_abs()),
            rewound,
        );
        self.publish();
        let Some(link) = &self.link else {
            return;
        };
        // At the hard limit, the checkpointer starts no pass while this
        // write checkpoints, nor for a while after in case the next write is
        // at the limit too: a pass starting the moment the last ended, as a
        // pass that commits raced does, would take the lock again before
        // this checkpoint could. Set before the nudge, which could start one.
        let started = Instant::now();
        link.passes.stand_aside_until(
            (frames >= HARD_LIMIT_FRAMES).then(|| started + CHECKPOINT_WAIT + RETRY),
        );
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
        if frames < HARD_LIMIT_FRAMES {
            // Past the soft limit the nudge has the checkpointer copy at
            // once; a checkpoint here would put its fsyncs inside the write.
            self.alarm = 2 * HARD_LIMIT_FRAMES;
            // A lockout only starts at the hard limit, so a log below it has
            // rewound since, and a checkpoint copied all of it.
            self.recovered();
            return;
        }
        // The checkpointer's passes keep being raced by commits, or a read
        // holds the log. Copy what is left here, where nothing can commit
        // meanwhile, so the next commit can rewind the log. If another
        // checkpoint holds the lock, wait for it rather than append past
        // the limit for as long as it runs; in a lockout, waiting would
        // hold every write for the whole wait, so try once.
        let (outcome, waited, copy) = if self.lockout.is_some() {
            let copying = Instant::now();
            (passive(conn), Duration::ZERO, copying.elapsed())
        } else {
            wait_and_checkpoint(conn, &link.passes)
        };
        link.passes.stand_aside_until(Some(Instant::now() + RETRY));
        self.stats.limit(CheckpointEvent::Limit {
            at: started,
            frames: u64::from(frames.unsigned_abs()),
            waited,
            copy,
            copied: match &outcome {
                Ok(done) if !done.busy => Some(done.copied.unsigned_abs()),
                _ => None,
            },
        });
        tracing::debug!(
            frames,
            ?outcome,
            took = ?started.elapsed(),
            "the writer checkpointed a log at its limit"
        );
        match &outcome {
            Ok(done) if !done.busy => self.recovered(),
            _ => self.locked_out(frames, started, &outcome),
        }
        if frames >= self.alarm {
            self.alarm = self.alarm.saturating_mul(2);
            if self.lockout.is_some() {
                tracing::error!(
                    frames,
                    ?outcome,
                    "the write-ahead log is past twice its limit and still growing; \
                     another checkpoint keeps holding it, or checkpoints keep failing"
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

    /// Starts a lockout, with its one error, or records how far the log
    /// has grown in the current one.
    fn locked_out(&mut self, frames: c_int, started: Instant, outcome: &rusqlite::Result<Outcome>) {
        if let Some(lockout) = &mut self.lockout {
            lockout.peak = lockout.peak.max(frames);
            return;
        }
        self.lockout = Some(Lockout {
            since: started,
            peak: frames,
        });
        self.stats.lockout();
        if let Err(error) = outcome {
            tracing::error!(
                frames,
                %error,
                "the writer's checkpoint of the write-ahead log failed; writes go past the \
                 log's limit until a checkpoint succeeds"
            );
        } else {
            tracing::error!(
                frames,
                "another checkpoint held the write-ahead log for longer than {CHECKPOINT_WAIT:?}; \
                 writes go past the log's limit, without waiting, until a checkpoint succeeds"
            );
        }
    }

    /// Ends a lockout, if there is one, with one line saying how it went.
    fn recovered(&mut self) {
        if let Some(Lockout { since, peak }) = self.lockout.take() {
            tracing::info!(
                lasted = ?since.elapsed(),
                peak_frames = peak,
                "checkpoints of the write-ahead log succeed again; the lockout is over"
            );
        }
    }
}

/// The writer's checkpoint at the limit. If another checkpoint holds the
/// lock, waits for it, up to [`CHECKPOINT_WAIT`], and tries again: on the
/// checkpointer's pass if one is running, otherwise (the holder is another
/// process) in steps of [`CHECKPOINT_POLL`].
///
/// Returns the outcome, the time spent waiting, and the time the last
/// checkpoint took.
///
/// This cannot deadlock. The checkpointer never waits on the writer: a pass
/// is one `PASSIVE` checkpoint, which never waits for a lock or calls a busy
/// handler, and the writer waits here between transactions, holding no
/// SQLite lock, and holding the passes' state only inside the condition
/// variable's wait, which releases it; a pass the checkpointer stands aside
/// from is skipped, not waited for. Every pass therefore ends, and the
/// deadline bounds the wait regardless.
fn wait_and_checkpoint(
    conn: &Connection,
    passes: &Passes,
) -> (rusqlite::Result<Outcome>, Duration, Duration) {
    let deadline = Instant::now() + CHECKPOINT_WAIT;
    let mut copying = Instant::now();
    let mut outcome = passive(conn);
    let mut copy = copying.elapsed();
    let mut waited = Duration::ZERO;
    #[cfg(test)]
    if locked_out(&outcome) {
        passes.waits.fetch_add(1, Ordering::Relaxed);
    }
    while locked_out(&outcome) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !passes.wait_out(deadline) {
            #[cfg(test)]
            passes.polls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(CHECKPOINT_POLL.min(deadline - now));
        }
        copying = Instant::now();
        waited += copying - now;
        outcome = passive(conn);
        copy = copying.elapsed();
    }
    (outcome, waited, copy)
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
        (
            Nudge::new(sender, Arc::default(), Arc::default(), Arc::default()),
            nudges,
        )
    }

    /// What this writer's checkpoints at the limit have done.
    pub(crate) fn stats(&self) -> CheckpointStats {
        self.stats.snapshot()
    }

    /// The passes the writer waits out; the test runs its own.
    pub(crate) fn passes(&self) -> Arc<Passes> {
        Arc::clone(&self.link.as_ref().expect("a linked nudge").passes)
    }
}

#[cfg(test)]
impl Passes {
    /// Writes at the limit that found the log locked and waited.
    pub(crate) fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// Steps of [`CHECKPOINT_POLL`] those writes slept.
    pub(crate) fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    /// Passes the checkpointer skipped to stand aside for the writer.
    pub(crate) fn skips(&self) -> u64 {
        self.skips.load(Ordering::Relaxed)
    }

    /// Passes the checkpointer ran.
    pub(crate) fn ran(&self) -> u64 {
        self.ran.load(Ordering::Relaxed)
    }
}

fn run(conn: &Connection, nudges: &Receiver<()>, log: &Log, passes: &Passes, stats: &Stats) {
    // The appended count the last checkpoint started from: frames past it
    // have not been copied.
    let mut copied_to = 0;
    // When the next checkpoint must run even below the threshold.
    let mut due: Option<Instant> = None;
    let mut stalled = 0;
    // The last pass copied every frame it found: no read held it back.
    let mut copied_all = true;
    // Passes in a row that ran only to chase the commits.
    let mut streak = 0;
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
        let target = log.appended.load(Ordering::Relaxed);
        let due_at = *due.get_or_insert(now + MAX_DELAY);
        // Past the soft limit, chase the commits: a pass that copied all it
        // found is followed at once by one for the frames committed during
        // it, until one that no commit raced lets the next commit rewind the
        // log. One that a read held back waits for `due`, as below.
        let past_soft_limit =
            log.frames.load(Ordering::Relaxed) >= u64::from(SOFT_LIMIT_FRAMES.unsigned_abs());
        let chase = past_soft_limit && copied_all && target > copied_to;
        let waiting = target - copied_to < THRESHOLD_FRAMES && now < due_at;
        if waiting && !chase {
            continue;
        }
        let Some(pass) = passes.begin() else {
            stats.stand_aside();
            #[cfg(test)]
            passes.skips.fetch_add(1, Ordering::Relaxed);
            // Try again once the writer has gone quiet; leaving `due` as it
            // was would spin while it is past.
            due = Some(Instant::now() + RETRY);
            continue;
        };
        copied_to = target;
        let started = Instant::now();
        let complete = {
            let _pass = pass;
            checkpoint(conn, &mut stalled)
        };
        streak = if waiting { streak + 1 } else { 0 };
        stats.pass(
            CheckpointEvent::Pass {
                at: started,
                took: started.elapsed(),
                chase: waiting,
                complete,
            },
            streak,
        );
        #[cfg(test)]
        passes.ran.fetch_add(1, Ordering::Relaxed);
        copied_all = complete;
        if complete {
            log.copied.fetch_max(copied_to, Ordering::Relaxed);
        }
        due = if !complete {
            Some(Instant::now() + RETRY)
        } else if log.appended.load(Ordering::Relaxed) > copied_to {
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
    conn.prepare_cached("PRAGMA wal_checkpoint(PASSIVE)")?
        .query_row([], |row| {
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
