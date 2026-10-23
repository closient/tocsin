// SPDX-License-Identifier: AGPL-3.0-only

//! A fixed pool of read connections for async callers.
//!
//! Hand-rolled rather than `r2d2`: the pool must be awaited, not block a
//! runtime thread, and r2d2 with `r2d2_sqlite` would add three crates for a
//! blocking `get()` we would wrap in `spawn_blocking` anyway. WAL mode lets
//! every reader run alongside the writer, so the pool is the whole story.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::Connection;
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::schema;

pub(crate) struct Readers {
    idle: Mutex<Vec<Connection>>,
    permits: Semaphore,
}

impl Readers {
    pub(crate) fn open(path: &Path, count: usize) -> Result<Arc<Readers>> {
        let idle = (0..count)
            .map(|_| schema::open_reader(path))
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Readers {
            idle: Mutex::new(idle),
            permits: Semaphore::new(count),
        }))
    }

    /// Runs `query` on an idle connection on the blocking thread pool,
    /// waiting for one to be free.
    pub(crate) async fn read<T, F>(self: &Arc<Self>, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let permit = self.permits.acquire().await.map_err(|_| Error::Closed)?;
        let Some(conn) = self.take() else {
            // A permit is only ever held alongside a connection.
            return Err(Error::Closed);
        };
        let ran = tokio::task::spawn_blocking(move || {
            let out = query(&conn);
            (conn, out)
        })
        .await;
        let Ok((conn, out)) = ran else {
            // The connection went down with the panic; shrink the pool to
            // match so a permit never promises a connection that is gone.
            permit.forget();
            return Err(Error::ReaderPanicked);
        };
        self.give_back(conn);
        drop(permit);
        out
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
}
