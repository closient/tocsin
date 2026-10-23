// SPDX-License-Identifier: AGPL-3.0-only

//! The checkpointer: copies the write-ahead log back into the database file
//! on its own connection and thread, so a checkpoint's fsyncs never run
//! inside a write.
//!
//! SQLite's automatic checkpoint runs inside whichever commit pushes the log
//! past 1000 pages, on the committing connection. Here that is the writer
//! thread, so every append queued behind that commit waited for the copy and
//! both fsyncs (20 to 480 ms on CI runners, TOC-57). The writer connection
//! has automatic checkpoints turned off and nudges this thread after each
//! write; this thread runs a `PASSIVE` checkpoint, which takes no lock the
//! writer or the readers need, at most once per [`INTERVAL`].

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::error::Result;

/// Least time between two checkpoints. At the 1000 pages SQLite would wait
/// for, a busy store checkpointed every few seconds; once a second keeps the
/// log smaller than that, off the write path.
const INTERVAL: Duration = Duration::from_secs(1);

/// SQLite's default `wal_autocheckpoint`, restored on the writer if this
/// thread is ever gone.
const AUTOCHECKPOINT_PAGES: i64 = 1000;

pub(crate) struct Checkpointer {
    thread: JoinHandle<()>,
}

/// The writer's end: tells the checkpointer a write committed.
pub(crate) struct Nudge(Option<SyncSender<()>>);

impl Checkpointer {
    /// Starts the checkpointer on `conn`. It stops once the returned
    /// [`Nudge`] is dropped, which happens when the writer thread ends.
    pub(crate) fn spawn(conn: Connection) -> Result<(Checkpointer, Nudge)> {
        // One pending nudge is enough: it means "the log has grown".
        let (sender, nudges) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("tocsin-store-checkpointer".into())
            .spawn(move || run(&conn, &nudges))?;
        Ok((Checkpointer { thread }, Nudge(Some(sender))))
    }

    /// Waits for the thread, which closes its connection on the way out.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }
}

impl Nudge {
    /// Called by the writer after each write. Never blocks.
    pub(crate) fn after_write(&mut self, conn: &Connection) {
        let Some(sender) = &self.0 else {
            return;
        };
        // Full means a nudge is already pending, which is all one says.
        if let Err(TrySendError::Disconnected(())) = sender.try_send(()) {
            // The thread can only end early by panicking. Without it the log
            // would grow without bound, so writes checkpoint it themselves
            // again, as SQLite does by default.
            self.0 = None;
            let restored = conn
                .pragma_update(None, "wal_autocheckpoint", AUTOCHECKPOINT_PAGES)
                .is_ok();
            tracing::error!(
                restored,
                "the checkpointer thread is gone; writes checkpoint the log themselves again"
            );
        }
    }
}

#[cfg(test)]
impl Nudge {
    /// A nudge with no checkpointer behind it; the test holds the other end.
    pub(crate) fn detached() -> (Nudge, Receiver<()>) {
        let (sender, nudges) = mpsc::sync_channel(1);
        (Nudge(Some(sender)), nudges)
    }
}

fn run(conn: &Connection, nudges: &Receiver<()>) {
    // Nothing to copy until the first write.
    if nudges.recv().is_err() {
        return;
    }
    loop {
        checkpoint(conn);
        // Let at least INTERVAL pass, remembering whether anything was
        // written meanwhile; stop as soon as the writer is gone.
        let next = Instant::now() + INTERVAL;
        let mut written = false;
        while let Some(left) = next.checked_duration_since(Instant::now()) {
            match nudges.recv_timeout(left) {
                Ok(()) => written = true,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if !written && nudges.recv().is_err() {
            return;
        }
    }
}

/// Copies as much of the log as no reader still needs. `PASSIVE` never
/// waits: frames a reader's snapshot still uses are left for the next pass,
/// and another process's checkpoint in progress just skips this one.
fn checkpoint(conn: &Connection) {
    let outcome = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
    });
    match outcome {
        Ok((log, copied)) => tracing::trace!(log, copied, "checkpointed the write-ahead log"),
        Err(error) => tracing::warn!(
            %error,
            "a write-ahead log checkpoint failed; the log grows until one succeeds"
        ),
    }
}
