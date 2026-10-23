// SPDX-License-Identifier: AGPL-3.0-only

//! Everything the engine needs from outside: running a probe, storing
//! history, reading incidents and maintenance, announcing changes and
//! telling the time. Each is a small trait so tests can substitute fakes;
//! the real implementations are here too.

use std::future::Future;
use std::pin::Pin;

use tocsin_core::probe::{CompiledCheck, ProbeOutcome};
use tocsin_probe::ProbeContext;
use tocsin_store::{NewCheckResult, NewOutage, Outage, Result, Store};

use crate::event::EngineEvent;
use crate::notice::Notice;

/// A boxed, sendable future, so the ports can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one check once.
pub trait Runner: Send + Sync + 'static {
    /// Probes `check` and judges the result. The engine applies the check's
    /// timeout around this call and stamps the result's start time and
    /// duration from its own clock.
    fn run<'a>(&'a self, check: &'a CompiledCheck) -> BoxFuture<'a, ProbeOutcome>;
}

/// The production [`Runner`]: the real probes.
#[derive(Debug, Clone)]
pub struct ProbeRunner {
    context: ProbeContext,
}

impl ProbeRunner {
    /// Probes with `context`'s trust roots and user agent.
    #[must_use]
    pub fn new(context: ProbeContext) -> Self {
        ProbeRunner { context }
    }
}

impl Runner for ProbeRunner {
    fn run<'a>(&'a self, check: &'a CompiledCheck) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(tocsin_probe::run(check, &self.context))
    }
}

/// Where results and outages are kept.
pub trait History: Send + Sync + 'static {
    /// Stores one raw result.
    fn append(&self, result: NewCheckResult) -> BoxFuture<'_, Result<i64>>;
    /// Opens an outage; resolves with its id.
    fn open_outage(&self, outage: NewOutage) -> BoxFuture<'_, Result<i64>>;
    /// Ends an open outage.
    fn end_outage(&self, id: i64, ended_at_ms: i64) -> BoxFuture<'_, Result<()>>;
    /// Every outage `node_id` still has open.
    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>>;
}

impl History for Store {
    fn append(&self, result: NewCheckResult) -> BoxFuture<'_, Result<i64>> {
        Box::pin(Store::append(self, result))
    }

    fn open_outage(&self, outage: NewOutage) -> BoxFuture<'_, Result<i64>> {
        Box::pin(Store::open_outage(self, outage))
    }

    fn end_outage(&self, id: i64, ended_at_ms: i64) -> BoxFuture<'_, Result<()>> {
        Box::pin(Store::end_outage(self, id, ended_at_ms))
    }

    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>> {
        Box::pin(Store::open_outages(self, node_id))
    }
}

/// Reads open incidents and maintenance windows that have not completed.
pub trait Notices: Send + Sync + 'static {
    /// Every unresolved incident and every maintenance window, scheduled or
    /// in progress, that has not completed. The engine works out which
    /// windows are active from their times.
    fn current(&self) -> BoxFuture<'_, Result<Vec<Notice>>>;
}

/// A [`Notices`] with nothing in it, for a node with no incident store.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoNotices;

impl Notices for NoNotices {
    fn current(&self) -> BoxFuture<'_, Result<Vec<Notice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Where the engine announces changes.
pub trait Sink: Send + Sync + 'static {
    /// Publishes one event. Called with the engine's state locked, so
    /// events arrive in the order the engine made the changes; it must not
    /// block.
    fn emit(&self, event: EngineEvent);
}

impl Sink for tokio::sync::broadcast::Sender<EngineEvent> {
    fn emit(&self, event: EngineEvent) {
        // An error only means nobody is subscribed right now; the change is
        // already recorded in the store and the engine's state.
        let _ = self.send(event);
    }
}

/// Wall-clock time for result timestamps and maintenance windows.
pub trait Clock: Send + Sync + 'static {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        tocsin_store::now_ms()
    }
}

/// A clock that follows tokio's clock from a fixed start, so tests on a
/// paused runtime see wall-clock time advance with `tokio::time::advance`.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    origin: tokio::time::Instant,
    origin_ms: i64,
}

impl TokioClock {
    /// A clock reading `origin_ms` now.
    #[must_use]
    pub fn starting_at(origin_ms: i64) -> Self {
        TokioClock {
            origin: tokio::time::Instant::now(),
            origin_ms,
        }
    }
}

impl Clock for TokioClock {
    fn now_ms(&self) -> i64 {
        let elapsed = i64::try_from(self.origin.elapsed().as_millis()).unwrap_or(i64::MAX);
        self.origin_ms.saturating_add(elapsed)
    }
}
