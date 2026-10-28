//! Who pages for another node's outage, for a node in `[federation]
//! alert_order` (docs/design/federation.md, "Who pages"): decided when the
//! push carrying it arrives, and decided again when the nodes ahead in the
//! list go down while it is still open, so an outage pushed while the node
//! ahead was up pages from this one once that node stops.

use std::collections::HashMap;
use std::sync::Arc;

use tocsin_alert::Router;
use tocsin_core::Health;
use tocsin_core::config::{Config, Id};
use tocsin_engine::{Snapshot, StatusHandle};
use tocsin_store::{NewOutage, RemoteAlerting, Store, SubjectKind};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, broadcast, watch};

use crate::http::events::Event;
use crate::liveness;

/// Where this node stands in `[federation] alert_order`, and what it needs
/// to decide how it alerts for another node's outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlertOrder {
    local: String,
    /// The nodes ahead of this one, in order.
    ahead: Vec<Id>,
    push_interval_ms: i64,
    grace_ms: i64,
    /// How long each check takes to confirm an outage: `confirm`
    /// intervals and a timeout.
    confirm_spans_ms: HashMap<String, i64>,
}

impl AlertOrder {
    /// This node's place in `config`'s `alert_order`; `None` when it is not
    /// in the list, which leaves other nodes' outages to the nodes that
    /// are.
    pub(crate) fn of(config: &Config) -> Option<Self> {
        let federation = config.federation.as_ref()?;
        let local = config.server.node_id.as_ref()?;
        let position = federation
            .alert_order
            .iter()
            .position(|node| node.as_str() == local)?;
        Some(Self {
            local: local.clone(),
            ahead: federation.alert_order[..position].to_vec(),
            push_interval_ms: millis(federation.push_interval.get()),
            grace_ms: millis(federation.grace.get()),
            confirm_spans_ms: config
                .checks
                .iter()
                .map(|check| {
                    let span = millis(check.interval.get()) * i64::from(check.confirm)
                        + millis(check.timeout.get());
                    (check.id.as_str().to_owned(), span)
                })
                .collect(),
        })
    }

    /// Whether every node ahead of this one is down as `snapshot` sees it:
    /// its `node:<id>` heartbeat unheard, or confirmed anything but up.
    /// `pusher`, the node whose batch is being applied, is up whatever its
    /// heartbeat says yet: the push is itself word from it, and its beat
    /// is recorded only after the batch.
    pub(crate) fn ahead_is_down(&self, snapshot: &Snapshot, pusher: Option<&str>) -> bool {
        self.ahead.iter().all(|node| {
            Some(node.as_str()) != pusher && node_liveness(snapshot, node) != Some(Health::Up)
        })
    }

    /// Whether this node takes over now: every node ahead is down, and at
    /// least one of them is confirmed so. A node merely unheard counts as
    /// down for a push, but cannot start a takeover on its own: right after
    /// this node starts every node is unheard until it pushes.
    fn takes_over(&self, snapshot: &Snapshot) -> bool {
        self.ahead_is_down(snapshot, None)
            && self.ahead.iter().any(|node| {
                node_liveness(snapshot, node).is_some_and(|health| health != Health::Up)
            })
    }

    /// Whether `check_id` is the `node:<id>` heartbeat of a node ahead.
    fn watches(&self, check_id: &str) -> bool {
        self.ahead
            .iter()
            .any(|node| Id::synthetic(liveness::NODE, node).as_str() == check_id)
    }

    /// How this node alerts for `outage`, which it heard of at
    /// `heard_at_ms`: routed as `router` routes it, opening alerts if
    /// `opens`. It is late when this node heard of it more than a push and
    /// its grace after the check could have confirmed it.
    pub(crate) fn alerting(
        &self,
        router: &Router,
        outage: &NewOutage,
        opens: bool,
        heard_at_ms: i64,
    ) -> RemoteAlerting {
        let confirm_span_ms = match outage.subject_kind {
            SubjectKind::Check => self
                .confirm_spans_ms
                .get(&outage.check_id)
                .copied()
                .unwrap_or(0),
            SubjectKind::Heartbeat => 0,
        };
        let due_by_ms =
            outage.started_at_ms + confirm_span_ms + self.push_interval_ms + self.grace_ms;
        RemoteAlerting {
            routing: router.routing(
                outage.subject_kind,
                &outage.check_id,
                outage.component_id.as_deref(),
                outage.health,
            ),
            opens,
            arrived_late_at_ms: (heard_at_ms > due_by_ms).then_some(heard_at_ms),
        }
    }
}

/// The confirmed health of `node`'s `node:<id>` heartbeat in `snapshot`;
/// `None` while unheard. The one test of whether a node is heard from, for
/// paging by `alert_order` and for showing its checks alike.
pub(crate) fn node_liveness(snapshot: &Snapshot, node: &Id) -> Option<Health> {
    let heartbeat = Id::synthetic(liveness::NODE, node);
    snapshot
        .checks
        .iter()
        .find(|check| check.id == heartbeat.as_str())
        .and_then(|check| check.state)
        .map(|confirmed| confirmed.health)
}

/// What a takeover reads from the running configuration; a reload sends a
/// new one.
pub(crate) struct Takeover {
    /// `None` when this node is not in `alert_order`.
    pub(crate) order: Option<AlertOrder>,
    pub(crate) router: Arc<Router>,
}

/// What a takeover runs against.
pub(crate) struct Watching {
    pub(crate) store: Arc<Store>,
    pub(crate) live: StatusHandle,
    pub(crate) alert_wake: Arc<Notify>,
}

/// Takes over other nodes' outages whenever the nodes ahead of this one in
/// `alert_order` go down: once at start, after every confirmed change of a
/// node ahead to anything but up, after a reload, and after missing events,
/// each time only if [`AlertOrder::takes_over`] holds then. Runs until the
/// event bus closes.
pub(crate) async fn take_over_when_ahead_is_down(
    mut events: broadcast::Receiver<Event>,
    mut config: watch::Receiver<Arc<Takeover>>,
    watching: Watching,
) {
    let takeover = Arc::clone(&config.borrow_and_update());
    take_over(&takeover, &watching).await;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::CheckChanged(change)) => {
                    let takeover = Arc::clone(&config.borrow());
                    if change.to != Health::Up
                        && takeover.order.as_ref().is_some_and(|order| order.watches(&change.check_id))
                    {
                        take_over(&takeover, &watching).await;
                    }
                }
                Ok(_) => {}
                // A node ahead may have gone down among the events missed.
                Err(RecvError::Lagged(_)) => {
                    let takeover = Arc::clone(&config.borrow());
                    take_over(&takeover, &watching).await;
                }
                Err(RecvError::Closed) => return,
            },
            changed = config.changed() => {
                if changed.is_err() {
                    return;
                }
                let takeover = Arc::clone(&config.borrow_and_update());
                take_over(&takeover, &watching).await;
            }
        }
    }
}

/// Opens alerts for every other node's outage this node holds open and
/// alerted without one, if every node ahead is down now. Late as
/// [`AlertOrder::alerting`] says, taking now as when this node heard of it:
/// it could not page for it before.
async fn take_over(takeover: &Takeover, watching: &Watching) {
    let Some(order) = &takeover.order else {
        return;
    };
    if !order.takes_over(&watching.live.snapshot()) {
        return;
    }
    let now_ms = tocsin_store::now_ms();
    let decide = {
        let order = order.clone();
        let router = Arc::clone(&takeover.router);
        move |outage: &NewOutage| order.alerting(&router, outage, true, now_ms)
    };
    match watching
        .store
        .take_over(order.local.clone(), decide, now_ms)
        .await
    {
        Ok(0) => {}
        Ok(queued) => {
            tracing::warn!(
                queued,
                "every node ahead of this one in alert_order is down: \
                 paging for the other nodes' outages they were paging for"
            );
            watching.alert_wake.notify_one();
        }
        Err(error) => tracing::error!(
            "cannot take over the other nodes' outages from the nodes ahead in alert_order: \
             {error}; the next change of a node ahead tries again"
        ),
    }
}

fn millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use tocsin_core::state::Confirmed;
    use tocsin_engine::{CheckSnapshot, MemberKind};

    use super::*;

    /// Third in `alert_order`, behind `first` and `second`.
    fn third() -> AlertOrder {
        AlertOrder {
            local: "third".into(),
            ahead: vec![Id::parse("first").unwrap(), Id::parse("second").unwrap()],
            push_interval_ms: 60_000,
            grace_ms: 30_000,
            confirm_spans_ms: HashMap::new(),
        }
    }

    /// What this node sees of `first` and `second`: each heartbeat confirmed
    /// at its health, or never heard when `None`.
    fn ahead(first: Option<Health>, second: Option<Health>) -> Snapshot {
        let heard = |node: &str, health: Option<Health>| {
            health.map(|health| CheckSnapshot {
                id: format!("node:{node}"),
                kind: MemberKind::Heartbeat,
                component_id: None,
                state: Some(Confirmed {
                    health,
                    since_ms: 0,
                }),
                last: None,
            })
        };
        Snapshot {
            checks: [heard("first", first), heard("second", second)]
                .into_iter()
                .flatten()
                .collect(),
            components: Vec::new(),
        }
    }

    #[test]
    fn a_node_takes_over_once_every_node_ahead_is_down_and_one_is_heard_so() {
        use Health::{Degraded, Down, Up};
        let cases = [
            (None, None, false),
            (Some(Down), None, true),
            (None, Some(Degraded), true),
            (Some(Down), Some(Down), true),
            (Some(Down), Some(Up), false),
            (Some(Up), None, false),
            (Some(Up), Some(Up), false),
        ];
        for (first, second, takes_over) in cases {
            assert_eq!(
                third().takes_over(&ahead(first, second)),
                takes_over,
                "first {first:?}, second {second:?}"
            );
        }
    }

    #[test]
    fn a_node_watches_only_the_heartbeats_of_the_nodes_ahead() {
        let order = third();
        assert!(order.watches("node:first"));
        assert!(order.watches("node:second"));
        assert!(!order.watches("node:third"));
        assert!(!order.watches("upstream:first"));
        assert!(!order.watches("api-port"));
    }
}
