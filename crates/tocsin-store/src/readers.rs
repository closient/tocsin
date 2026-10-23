// SPDX-License-Identifier: AGPL-3.0-only

//! A fixed pool of read connections for async callers.
//!
//! Hand-rolled rather than `r2d2`: the pool must be awaited, not block a
//! runtime thread, and r2d2 with `r2d2_sqlite` would add three crates for a
//! blocking `get()` we would wrap in `spawn_blocking` anyway. WAL mode lets
//! every reader run alongside the writer, so the pool is the whole story.
//!
//! A borrowed connection travels inside a [`Lease`] that returns it when
//! dropped, and the lease lives on the blocking thread, not in the caller's
//! future. A caller that stops waiting (a timeout, a `select!`, an abort)
//! therefore cannot lose a connection, and a query that panics has its
//! connection replaced rather than its slot forgotten.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{Error, Result};
use crate::schema;

pub(crate) struct Readers {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
    /// Connections the pool still owns, idle or leased.
    live: Mutex<usize>,
}

impl Readers {
    pub(crate) fn open(path: &Path, count: usize) -> Result<Arc<Readers>> {
        let idle = (0..count)
            .map(|_| schema::open_reader(path))
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Readers {
            path: path.to_path_buf(),
            idle: Mutex::new(idle),
            permits: Arc::new(Semaphore::new(count)),
            live: Mutex::new(count),
        }))
    }

    /// Runs `query` on an idle connection on the blocking thread pool,
    /// waiting for one to be free.
    pub(crate) async fn read<T, F>(self: &Arc<Self>, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| Error::Closed)?;
        let Some(conn) = self.take() else {
            // A permit is only ever held alongside a connection.
            return Err(Error::Closed);
        };
        let lease = Lease {
            readers: Arc::clone(self),
            conn: Some(conn),
            permit: Some(permit),
        };
        tokio::task::spawn_blocking(move || lease.run(query))
            .await
            .unwrap_or(Err(Error::ReaderPanicked))
    }

    fn take(&self) -> Option<Connection> {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
    }

    fn give_back(&self, conn: Connection) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(conn);
    }

    /// Gives up one connection slot for good. When none is left, closes the
    /// semaphore so every waiting and future read fails instead of hanging.
    fn shrink(&self, permit: OwnedSemaphorePermit) {
        permit.forget();
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        *live -= 1;
        if *live == 0 {
            self.permits.close();
        }
    }
}

/// One borrowed connection and the permit that reserves it. Dropping the
/// lease, wherever that happens, puts the connection back before the permit
/// is released.
struct Lease {
    readers: Arc<Readers>,
    conn: Option<Connection>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Lease {
    fn run<T>(mut self, query: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let Some(conn) = self.conn.as_ref() else {
            return Err(Error::Closed);
        };
        let Ok(out) = panic::catch_unwind(AssertUnwindSafe(|| query(conn))) else {
            self.replace_connection();
            return Err(Error::ReaderPanicked);
        };
        out
    }

    /// A query that panicked may have left the connection mid-statement, so
    /// it is closed and a fresh one takes its slot. If SQLite will not open
    /// another, the pool shrinks by one instead.
    fn replace_connection(&mut self) {
        self.conn = None;
        match schema::open_reader(&self.readers.path) {
            Ok(fresh) => self.conn = Some(fresh),
            Err(_) => {
                if let Some(permit) = self.permit.take() {
                    self.readers.shrink(permit);
                }
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.readers.give_back(conn);
        }
        drop(self.permit.take());
    }
}
