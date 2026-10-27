// SPDX-License-Identifier: AGPL-3.0-only

//! The single writer: one thread owns the only write connection and takes
//! work from a channel, so no two writes ever contend for SQLite's lock.
//!
//! A panic in a write is caught, the open transaction (if any) is rolled
//! back, the waiting callers get [`Error::WriterPanicked`] and the thread
//! carries on: a bug in one write must not silently stop every later one.
//!
//! Every write is answered once it has committed, except a probe result
//! that changes nothing ([`Writer::queue`]), which is answered once queued:
//! the first commit after a checkpoint has copied the whole log syncs the
//! log's new header inside the commit, and on a busy disk that sync has
//! taken 442 ms (TOC-66, DESIGN.md "Durability").

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use rusqlite::{Connection, TransactionBehavior, params};
use tocsin_core::status::OutageEnding;
use tokio::sync::{mpsc, oneshot};

use crate::checkpoint::Nudge;
use crate::error::{Error, Result};
use crate::node::take_seq;
use crate::{NewCheckResult, OutageChange, SubjectKind};

/// Queued writes beyond this make callers wait (backpressure).
const QUEUE_DEPTH: usize = 1024;

/// Most inserts committed in one transaction.
const MAX_BATCH: usize = 256;

/// A small row write. Consecutive queued inserts share one transaction.
pub(crate) enum Insert {
    /// A result, and the outage change it confirms if any, kept or rejected
    /// together.
    Record {
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    },
    ChangeOutage(OutageChange),
}

/// Any other write; it runs alone and returns its reply, which the writer
/// sends once it has published the log's length.
type Job = Box<dyn FnOnce(&mut Connection) -> Reply + Send>;

/// Sends a job's outcome to its caller.
type Reply = Box<dyn FnOnce() + Send>;

enum Command {
    /// Boxed: a result with its outage change dwarfs a job.
    Insert(Box<Queued>),
    Run(Job),
}

/// An insert, when it was queued, and who to answer once it commits: no one
/// if it was answered as it was queued.
struct Queued {
    insert: Insert,
    at: Instant,
    reply: Option<oneshot::Sender<Result<i64>>>,
}

pub(crate) struct Writer {
    sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
    /// Writes queued so far.
    queued: AtomicU64,
}

impl Writer {
    pub(crate) fn spawn(conn: Connection, nudge: Nudge) -> Result<Writer> {
        let (sender, receiver) = mpsc::channel(QUEUE_DEPTH);
        let thread = std::thread::Builder::new()
            .name("tocsin-store-writer".into())
            .spawn(move || run(Session { conn, nudge }, receiver))?;
        Ok(Writer {
            sender,
            thread,
            queued: AtomicU64::new(0),
        })
    }

    /// How many inserts and jobs have been queued since the writer started.
    pub(crate) fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// Queues an insert; resolves once its transaction has committed with
    /// the outage it opened, or else the outage it ended, or else (a
    /// `Record` with no change) the result's row id.
    pub(crate) async fn insert(&self, insert: Insert) -> Result<i64> {
        let (reply, receive) = oneshot::channel();
        self.send(insert, Some(reply)).await?;
        receive.await.map_err(|_| Error::Closed)?
    }

    /// Queues an insert and resolves at once, waiting only for room in a
    /// full queue: the caller gives up hearing whether it committed. One
    /// the database refuses is logged, and counted in
    /// [`crate::CheckpointStats::rejected`].
    pub(crate) async fn queue(&self, insert: Insert) -> Result<()> {
        self.send(insert, None).await
    }

    async fn send(
        &self,
        insert: Insert,
        reply: Option<oneshot::Sender<Result<i64>>>,
    ) -> Result<()> {
        self.queued.fetch_add(1, Ordering::Relaxed);
        let queued = Queued {
            insert,
            at: Instant::now(),
            reply,
        };
        self.sender
            .send(Command::Insert(Box::new(queued)))
            .await
            .map_err(|_| Error::Closed)
    }

    /// Runs `job` on the write connection, after every write queued before it.
    pub(crate) async fn run<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply, receive) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let outcome = guarded(conn, |conn| job(conn));
            Box::new(move || {
                // The caller may have stopped waiting; the write still happened.
                let _ = reply.send(outcome);
            })
        });
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(Command::Run(job))
            .await
            .map_err(|_| Error::Closed)?;
        receive.await.map_err(|_| Error::Closed)?
    }

    /// Runs `job` on the writer thread with no guard, so a panic in it ends
    /// the thread.
    #[cfg(test)]
    pub(crate) async fn run_unguarded(&self, job: impl FnOnce(&mut Connection) + Send + 'static) {
        let job: Job = Box::new(move |conn| {
            job(conn);
            Box::new(|| ())
        });
        let _ = self.sender.send(Command::Run(job)).await;
    }

    /// Stops taking work, lets the queue drain and returns the thread, which
    /// finishes once the connection is closed.
    pub(crate) fn close(self) -> JoinHandle<()> {
        drop(self.sender);
        self.thread
    }
}

/// The writer thread's connection and its nudge. Only the last connection
/// to close checkpoints and removes the log, and the checkpointer closes its
/// own once the nudge is gone, so the connection must close first or the two
/// can close together and both leave the log behind. Fields drop in
/// declaration order, on return and on unwind alike.
struct Session {
    conn: Connection,
    nudge: Nudge,
}

fn run(mut session: Session, mut receiver: mpsc::Receiver<Command>) {
    // Kept whole, not destructured, so the field order decides the drops.
    let Session { conn, nudge } = &mut session;
    let mut next = None;
    loop {
        let Some(command) = next.take().or_else(|| receiver.blocking_recv()) else {
            break;
        };
        nudge.before_write();
        match command {
            Command::Run(job) => {
                // Retention acts on the log's length as soon as its job
                // replies, so the checkpointer learns the length first.
                let reply = job(conn);
                nudge.wrote();
                reply();
            }
            Command::Insert(queued) => {
                let mut batch = vec![*queued];
                while batch.len() < MAX_BATCH {
                    match receiver.try_recv() {
                        Ok(Command::Insert(queued)) => batch.push(*queued),
                        Ok(other) => {
                            next = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let (inserts, answers): (Vec<_>, Vec<_>) = batch
                    .into_iter()
                    .map(|queued| (queued.insert, (queued.at, queued.reply)))
                    .unzip();
                let outcomes = commit_batch(conn, &inserts);
                // Counted before anyone is answered, so a caller sees its own
                // write's rewind in the stats (TOC-70).
                nudge.wrote();
                nudge
                    .counters()
                    .committed(answers.iter().map(|&(at, _)| at), Instant::now());
                let mut refused = Vec::new();
                let mut replies = Vec::new();
                for ((insert, (_, reply)), outcome) in inserts.iter().zip(answers).zip(outcomes) {
                    match reply {
                        Some(reply) => replies.push((reply, outcome)),
                        None => {
                            if let Err(error) = outcome {
                                refused.push((insert, error));
                            }
                        }
                    }
                }
                // Counted before anyone in the batch is answered, so a caller
                // answered after a refused result sees it counted.
                refuse(nudge, &refused);
                for (reply, outcome) in replies {
                    let _ = reply.send(outcome);
                }
            }
        }
        nudge.after_write(conn);
    }
}

/// Logs the probe results answered on queueing that the database then
/// refused, one line for each unless a whole batch went: then one in all.
fn refuse(nudge: &Nudge, refused: &[(&Insert, Error)]) {
    let Some((_, first)) = refused.first() else {
        return;
    };
    nudge
        .counters()
        .rejected(u64::try_from(refused.len()).unwrap_or(u64::MAX));
    if matches!(first, Error::Batch(_) | Error::WriterPanicked) {
        tracing::error!(
            results = refused.len(),
            error = %first,
            "a batch of probe results already acknowledged failed to commit; they are not stored"
        );
        return;
    }
    for (insert, error) in refused {
        if let Insert::Record { result, .. } = insert {
            tracing::error!(
                check = %result.check_id,
                node = %result.node_id,
                started_at_ms = result.started_at_ms,
                %error,
                "a probe result already acknowledged was refused; it is not stored"
            );
        }
    }
}

/// Runs `write`, turning a panic into [`Error::WriterPanicked`] after
/// rolling back whatever transaction it left open.
fn guarded<T>(
    conn: &mut Connection,
    write: impl FnOnce(&mut Connection) -> Result<T>,
) -> Result<T> {
    let Ok(outcome) = panic::catch_unwind(AssertUnwindSafe(|| write(conn))) else {
        let rolled_back = conn.is_autocommit() || conn.execute_batch("ROLLBACK").is_ok();
        tracing::error!(
            rolled_back,
            "a database write panicked; its callers get an error and the writer carries on"
        );
        return Err(Error::WriterPanicked);
    };
    outcome
}

/// Applies each insert under its own savepoint, so one rejected row does not
/// sink the others, then commits once. Returns one outcome per insert, each
/// true of what is in the database afterwards.
fn commit_batch(conn: &mut Connection, inserts: &[Insert]) -> Vec<Result<i64>> {
    let committed = guarded(conn, |conn| Ok(commit_inserts(conn, inserts)?));
    match committed {
        Ok(outcomes) => outcomes
            .into_iter()
            .map(|outcome| outcome.map_err(Error::from))
            .collect(),
        Err(Error::WriterPanicked) => inserts.iter().map(|_| Err(Error::WriterPanicked)).collect(),
        Err(error) => {
            let message = error.to_string();
            inserts
                .iter()
                .map(|_| Err(Error::Batch(message.clone())))
                .collect()
        }
    }
}

fn commit_inserts(
    conn: &mut Connection,
    inserts: &[Insert],
) -> rusqlite::Result<Vec<rusqlite::Result<i64>>> {
    let mut transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut outcomes = Vec::with_capacity(inserts.len());
    for insert in inserts {
        let savepoint = transaction.savepoint()?;
        let outcome = apply(&savepoint, insert);
        match &outcome {
            Ok(_) => savepoint.commit()?,
            // Rolls back to the savepoint, undoing only this insert.
            Err(_) => drop(savepoint),
        }
        // Some failures (SQLITE_FULL, IOERR, NOMEM, a RAISE(ROLLBACK))
        // roll back the whole transaction, not just the savepoint. Carrying
        // on would open a fresh implicit transaction and report outcomes
        // that do not match what committed, so the whole batch fails.
        if transaction.is_autocommit() {
            return Err(outcome.err().unwrap_or_else(|| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ABORT),
                    Some("the batch's transaction was rolled back".into()),
                )
            }));
        }
        outcomes.push(outcome);
    }
    transaction.commit()?;
    Ok(outcomes)
}

fn apply(conn: &Connection, insert: &Insert) -> rusqlite::Result<i64> {
    match insert {
        // Both halves run under the insert's savepoint: either fails and
        // neither is kept.
        Insert::Record {
            result,
            kind,
            change,
        } => {
            let id = insert_result(conn, result, *kind)?;
            match change {
                Some(change) => apply_outage_change(conn, change),
                None => Ok(id),
            }
        }
        Insert::ChangeOutage(change) => apply_outage_change(conn, change),
    }
}

pub(crate) fn insert_result(
    conn: &Connection,
    result: &NewCheckResult,
    kind: SubjectKind,
) -> rusqlite::Result<i64> {
    let detail = result.detail.as_ref().map(ToString::to_string);
    conn.prepare_cached(
        "INSERT INTO check_result
             (node_id, check_id, started_at, duration_ms, health, detail, error, status_code, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?
    .execute(params![
        result.node_id,
        result.check_id,
        result.started_at_ms,
        result.duration_ms,
        result.health.as_str(),
        detail,
        result.error,
        result.status_code,
        kind.as_str(),
    ])?;
    Ok(conn.last_insert_rowid())
}

/// Ends and opens outages as `change` says, inside whatever transaction or
/// savepoint `conn` is in. Returns the opened outage's id, or with none the
/// ended one's. Every change is one the engine saw, so an ended outage
/// ended [`OutageEnding::Recovered`].
pub(crate) fn apply_outage_change(
    conn: &Connection,
    change: &OutageChange,
) -> rusqlite::Result<i64> {
    let mut id = 0;
    if let Some(end) = &change.end {
        let changed = conn
            .prepare_cached(
                "UPDATE outage SET ended_at = ?2, ending = ?3, ended_seq = ?4 WHERE id = ?1",
            )?
            .execute(params![
                end.id,
                end.ended_at_ms,
                OutageEnding::Recovered.as_str(),
                take_seq(conn)?,
            ])?;
        if changed == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        id = end.id;
    }
    if let Some(outage) = &change.open {
        conn.prepare_cached(
            "INSERT INTO outage (node_id, subject_kind, check_id, component_id, health,
                 started_at, alerted, opened_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            outage.node_id,
            outage.subject_kind.as_str(),
            outage.check_id,
            outage.component_id,
            outage.health.as_str(),
            outage.started_at_ms,
            outage.alerted,
            take_seq(conn)?,
        ])?;
        id = conn.last_insert_rowid();
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::QUEUE_DEPTH;
    use crate::{DB_FILE, NewCheckResult, Store, SubjectKind, now_ms};

    fn result() -> NewCheckResult {
        NewCheckResult {
            node_id: "node-a".into(),
            check_id: "web".into(),
            started_at_ms: now_ms(),
            duration_ms: 3,
            health: tocsin_core::Health::Up,
            detail: None,
            error: None,
            status_code: None,
        }
    }

    async fn stored(store: &Store) -> i64 {
        store
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM check_result", [], |row| row.get(0))?)
            })
            .await
            .unwrap()
    }

    /// A probe result answered on queueing still waits for room in a full
    /// queue: the bound is backpressure, and nothing is dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_queue_makes_a_probe_result_wait() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join(DB_FILE)).unwrap());
        // Parks the writer thread in a job until released.
        let (parked, is_parked) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let job = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .writer
                    .run(move |_| {
                        let _ = parked.send(());
                        let _ = released.recv();
                        Ok(())
                    })
                    .await
            })
        };
        is_parked.await.unwrap();

        for _ in 0..QUEUE_DEPTH {
            tokio::time::timeout(
                Duration::from_secs(5),
                store.record(result(), SubjectKind::Check, None),
            )
            .await
            .expect("room in the queue")
            .unwrap();
        }
        let waiting = {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.record(result(), SubjectKind::Check, None).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !waiting.is_finished(),
            "a record into a full queue waits for room"
        );

        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        waiting.await.unwrap().unwrap();
        // Queued behind every one of them, so answered after their commits.
        store.append(result()).await.unwrap();
        let queued = i64::try_from(QUEUE_DEPTH).unwrap();
        assert_eq!(stored(&store).await, queued + 2, "every result was stored");
        assert_eq!(store.checkpoint_stats().rejected, 0);
        Arc::into_inner(store).unwrap().close().await.unwrap();
    }

    /// A probe result the database refuses after it was answered is counted
    /// (and logged); a write answered on its commit gets the error instead.
    #[tokio::test]
    async fn a_refused_probe_result_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let mut refused = result();
        refused.node_id = String::new();

        store
            .record(refused.clone(), SubjectKind::Check, None)
            .await
            .unwrap();
        store.append(result()).await.unwrap();
        assert_eq!(store.checkpoint_stats().rejected, 1);
        assert_eq!(stored(&store).await, 1, "only the valid result was stored");

        assert!(store.append(refused).await.is_err());
        assert_eq!(store.checkpoint_stats().rejected, 1);
        store.close().await.unwrap();
    }
}
