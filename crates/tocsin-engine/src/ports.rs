// SPDX-License-Identifier: AGPL-3.0-only

//! Everything the engine needs from outside: running a probe, storing
//! history, reading incidents and maintenance, announcing changes and
//! telling the time. Each is a small trait so tests can substitute fakes;
//! the real implementations are here too.

use std::future::Future;
use std::pin::Pin;

use tocsin_core::probe::{CompiledCheck, ProbeOutcome};
use tocsin_probe::ProbeContext;
use tocsin_store::{
    LastUp, LatestResult, NewCheckResult, Outage, OutageChange, Result, Store, SubjectKind,
};

use crate::event::EngineEvent;
use crate::notice::Notice;

/// A boxed, sendable future, so the ports can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one check once.
pub trait Runner: Send + Sync + 'static {
    /// Probes `check` and judges the result, within the check's timeout,
    /// stamping the outcome with when the probe started and how long it
    /// took. The engine records those stamps as they are; it abandons a run
    /// only well past the timeout, as a bug (see
    /// [`PROBE_BACKSTOP`](crate::PROBE_BACKSTOP)).
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
    /// Stores one raw result of `kind` and the outage change it confirms,
    /// if any (ending an outage, opening one, or both), in one transaction:
    /// all of it happens or none does. Resolves with the opened outage's id,
    /// if one was opened.
    fn record(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    ) -> BoxFuture<'_, Result<Option<i64>>>;
    /// Every outage `node_id` still has open.
    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>>;
    /// For each of `check_ids` with an `up` result on `node_id` since its
    /// last outage: when that result started, and since when the check has
    /// been up.
    fn last_up<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LastUp>>>;
    /// For each of `check_ids` with any result on `node_id`: when the
    /// latest one started.
    fn latest_results<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LatestResult>>>;
    /// Ends outage `outage`, whose check or heartbeat is no longer
    /// configured, at `at_ms`, and ends any alert it fired with a resolve
    /// that says the configuration ended it: nobody saw it recover. Ending
    /// one that is not open does nothing.
    fn withdraw(&self, outage: i64, at_ms: i64) -> BoxFuture<'_, Result<()>>;
}

impl History for Store {
    fn record(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    ) -> BoxFuture<'_, Result<Option<i64>>> {
        Box::pin(Store::record(self, result, kind, change))
    }

    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>> {
        Box::pin(Store::open_outages(self, node_id))
    }

    fn last_up<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LastUp>>> {
        Box::pin(Store::last_up(self, node_id, check_ids))
    }

    fn latest_results<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LatestResult>>> {
        Box::pin(Store::latest_results(self, node_id, check_ids))
    }

    fn withdraw(&self, outage: i64, at_ms: i64) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { Store::withdraw_outage(self, outage, at_ms).await.map(drop) })
    }
}

/// Reads open incidents and maintenance windows that have not completed.
pub trait Notices: Send + Sync + 'static {
    /// Every unresolved incident and every maintenance window, scheduled or
    /// in progress, that has not completed. The engine works out which
    /// windows are active from their times.
    fn current(&self) -> BoxFuture<'_, Result<Vec<Notice>>>;
}

impl Notices for Store {
    fn current(&self) -> BoxFuture<'_, Result<Vec<Notice>>> {
        Box::pin(async {
            let incidents = self.open_incidents().await?;
            Ok(incidents.iter().filter_map(Notice::from_open).collect())
        })
    }
}

/// Where the engine announces changes.
pub trait Sink: Send + Sync + 'static {
    /// Publishes one event. Called with the engine's state locked, so
    /// events arrive in the order the engine made the changes; it must not
    /// block.
    fn emit(&self, event: EngineEvent);
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
