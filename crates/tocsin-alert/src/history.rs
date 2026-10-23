// SPDX-License-Identifier: AGPL-3.0-only

//! The engine's [`History`], with each outage change's alerting deliveries
//! written in the same transaction.

use std::sync::Arc;

use tocsin_engine::ports::{BoxFuture, Clock, History};
use tocsin_store::{
    LastUp, LatestResult, NewCheckResult, Outage, OutageChange, Result, Store, SubjectKind,
};
use tokio::sync::Notify;

use crate::route::Router;

/// A [`Store`] as the engine's history, queueing alert deliveries as
/// outages change.
///
/// It takes the engine's decision as given: an opened outage marked
/// `alerted` fires on every notifier its routes name, and a recovery
/// resolves whatever its predecessor fired. It never looks at maintenance
/// windows; the engine already did.
pub struct AlertingHistory {
    store: Arc<Store>,
    router: Arc<Router>,
    wake: Arc<Notify>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for AlertingHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertingHistory").finish_non_exhaustive()
    }
}

impl AlertingHistory {
    /// Writes to `store`, routing with `router` and waking the delivery
    /// worker through `wake` when something was queued.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        router: Arc<Router>,
        wake: Arc<Notify>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        AlertingHistory {
            store,
            router,
            wake,
            clock,
        }
    }
}

impl History for AlertingHistory {
    fn record(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    ) -> BoxFuture<'_, Result<Option<i64>>> {
        Box::pin(async move {
            // Most results change nothing: those join the writer's batches.
            let Some(change) = change else {
                return self.store.record(result, kind, None).await;
            };
            let routing = change
                .open
                .as_ref()
                .filter(|open| open.alerted)
                .map(|open| {
                    self.router.routing(
                        open.subject_kind,
                        &open.check_id,
                        open.component_id.as_deref(),
                        open.health,
                    )
                });
            let alerts = self
                .store
                .record_alerting(result, kind, change, routing, self.clock.now_ms())
                .await?;
            if alerts.queued > 0 {
                self.wake.notify_one();
            }
            Ok(alerts.opened)
        })
    }

    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>> {
        Box::pin(self.store.open_outages(node_id))
    }

    fn last_up<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LastUp>>> {
        Box::pin(self.store.last_up(node_id, check_ids))
    }

    fn latest_results<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LatestResult>>> {
        Box::pin(self.store.latest_results(node_id, check_ids))
    }
}
