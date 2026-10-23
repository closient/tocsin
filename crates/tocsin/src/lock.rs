// SPDX-License-Identifier: AGPL-3.0-only

//! The data folder's lock file. `tocsin serve` holds an exclusive lock on
//! it for as long as it runs, so a second server, or a `tocsin db vacuum`
//! that would stall its writes for the length of the rebuild, is refused
//! instead of fighting it for the database. Commands that only read, or
//! that queue behind the writer like any other write (`db info`,
//! `db rollup`), do not take it.
//!
//! The lock is the operating system's (`flock` on Linux and macOS), so it
//! goes away with the process however that ends; a stale file is harmless.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::exit::Exit;
use crate::output::Failure;

/// The lock file's name inside the data folder.
pub const LOCK_FILE: &str = "tocsin.lock";

/// Takes the data folder's lock, or returns `Ok(None)` when another process
/// holds it. The lock lasts as long as the returned file is open.
pub fn try_take(data: &Path) -> Result<Option<File>, Failure> {
    let path = data.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("cannot open lock file {}: {error}", path.display()),
            )
        })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(Failure::new(
            Exit::Runtime,
            format!("cannot lock {}: {error}", path.display()),
        )),
    }
}
