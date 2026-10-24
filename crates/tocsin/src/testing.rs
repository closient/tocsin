// SPDX-License-Identifier: AGPL-3.0-only

//! Helpers shared by the unit tests.

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// The log lines written on a thread while it holds [`Captured::capture`]'s
/// guard.
///
/// One subscriber, the global default, writes each line to the `Captured`
/// its thread holds. A subscriber per test, set with `set_default`, loses
/// lines: tracing caches whether each log line is wanted once for the whole
/// process, and a thread with no subscriber that reaches a line first, while
/// one test's subscriber is the only one registered, caches "never" for
/// every thread.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static CAPTURING: RefCell<Option<Captured>> = const { RefCell::new(None) };
}

impl Captured {
    /// Captures what this thread logs, at `info` and above, until the guard
    /// is dropped.
    #[must_use = "capturing stops when the guard is dropped"]
    pub fn capture(&self) -> Capturing {
        static ROUTER: Once = Once::new();
        ROUTER.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(Router)
                    .with_ansi(false)
                    .finish(),
            )
            .expect("nothing else sets a global subscriber in the unit tests");
        });
        // A line first reached before the subscriber became the default
        // cached "never". Tracing registers a line without a lock while one
        // subscriber exists, so a thread descheduled mid-registration can
        // store that "never" after a rebuild. Only a line first reached
        // before the default was set can, so rebuilding on every capture
        // leaves just the captures that start while such a registration is
        // still pending, around the first, to miss it.
        tracing_core::callsite::rebuild_interest_cache();
        Capturing(CAPTURING.replace(Some(self.clone())))
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Restores the thread's previous capture, if any, when dropped.
pub struct Capturing(Option<Captured>);

impl Drop for Capturing {
    fn drop(&mut self) {
        CAPTURING.set(self.0.take());
    }
}

/// The global subscriber's writer: the thread's [`Captured`], or nowhere.
struct Router;

/// A line for the thread's [`Captured`], or for nowhere.
struct Routed(Option<Captured>);

impl Write for Routed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            Some(captured) => captured.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Router {
    type Writer = Routed;

    fn make_writer(&'a self) -> Self::Writer {
        Routed(CAPTURING.with_borrow(Option::clone))
    }
}
