// SPDX-License-Identifier: AGPL-3.0-only

//! Nodes watching each other (`docs/design/federation.md`).
//!
//! A push doubles as its sender's heartbeat, so each node keeps one
//! heartbeat for every node that pushes to it, `node:<id>`, pinged by the
//! pushes it takes, and one for every node it pushes to, `upstream:<id>`,
//! pinged by the pushes that go through. A node that stops pushing, or an
//! upstream that stops taking pushes, misses its heartbeat and opens an
//! outage, which alerts like any other, and the next push resolves it.
//!
//! They are added as `tocsin serve` loads the configuration, never by
//! `config check` or `config show`: no file names them. Their ids hold a
//! colon, which no configured id may, so they never collide with one,
//! and neither a ping nor a `[[route]]` can name them. A silence can, to
//! quiet a node taken down on purpose.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::config::{Config, Heartbeat, Id, ProbeClass};
use tocsin_engine::{HeartbeatHandle, Ping, PingError};
use tokio::time::Instant;

use crate::metrics::CheckMetrics;

/// The kind of a heartbeat watching a node that pushes here.
pub(crate) const NODE: &str = "node";

/// The kind of a heartbeat watching a node this node pushes to.
pub(crate) const UPSTREAM: &str = "upstream";

/// Adds the liveness heartbeats to `config`, a configuration `tocsin
/// serve` is about to run: `node:<id>` for each node, not retired and not
/// this one, whose `push_to` names this node, and `upstream:<id>` for each
/// of `upstreams`, the nodes this node's pushers push to. Those are read
/// only at startup, so a reload passes the ones startup read.
///
/// Each is expected every `push_interval` and is down once `[federation]
/// grace` passes on top without a push. It has no component, so it is on
/// the internal page and never the public one.
pub(crate) fn watch<'a>(config: &mut Config, upstreams: impl IntoIterator<Item = &'a str>) {
    let (Some(federation), Some(local)) = (&config.federation, &config.server.node_id) else {
        return;
    };
    let heartbeat = |kind: &str, node: &Id, name: String| Heartbeat {
        id: Id::synthetic(kind, node),
        name: Some(name),
        component: None,
        interval: federation.push_interval,
        grace: federation.grace,
        links: Vec::new(),
        class: ProbeClass::default(),
        topology_node: None,
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
    };
    let pushers = config.nodes.iter().filter(|node| {
        !node.retired
            && node.id.as_str() != local
            && node.push_to.iter().any(|target| target.as_str() == local)
    });
    let mut watches: Vec<Heartbeat> = pushers
        .map(|node| heartbeat(NODE, &node.id, format!("Pushes from {}", node.id)))
        .collect();
    watches.extend(
        upstreams
            .into_iter()
            .filter_map(|upstream| Id::parse(upstream).ok())
            .map(|node| heartbeat(UPSTREAM, &node, format!("Pushes to {node}"))),
    );
    config.heartbeats.extend(watches);
}

/// When each liveness heartbeat was last pinged, and with what health.
/// Cheap to clone; clones share it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pulse(Arc<Mutex<HashMap<String, (Health, Instant)>>>);

impl Pulse {
    /// Pings heartbeat `id` up for a push that went through: see
    /// [`Pulse::pulse`].
    pub(crate) async fn beat(
        &self,
        heartbeats: &HeartbeatHandle,
        metrics: &CheckMetrics,
        id: &str,
        push_interval: Duration,
    ) {
        let ping = Ping {
            health: Health::Up,
            message: None,
        };
        self.pulse(heartbeats, metrics, id, ping, push_interval)
            .await;
    }

    /// Pings heartbeat `id` with `ping`, unless it was pinged with the same
    /// health in the last half `push_interval`. Each ping is a stored
    /// result, and a backlog sends pushes back to back, so this keeps it to
    /// two a `push_interval` while a push every `push_interval` still pings
    /// every time; a change of health always pings. A ping that is not
    /// stored is logged and tried again at the next push; a heartbeat no
    /// watch is kept for, such as one from a node whose `push_to` does not
    /// name this one, is ignored.
    pub(crate) async fn pulse(
        &self,
        heartbeats: &HeartbeatHandle,
        metrics: &CheckMetrics,
        id: &str,
        ping: Ping,
        push_interval: Duration,
    ) {
        let now = Instant::now();
        let health = ping.health;
        let due = self.lock().get(id).is_none_or(|(last_health, last)| {
            *last_health != health || now.duration_since(*last) >= push_interval / 2
        });
        if !due {
            return;
        }
        match heartbeats.ping(id, ping).await {
            Ok(at_ms) => {
                self.lock().insert(id.to_owned(), (health, now));
                metrics.pinged(id, at_ms);
            }
            Err(PingError::Unknown | PingError::Stopped) => {}
            Err(error @ PingError::NotStored) => {
                tracing::warn!(heartbeat = id, %error, "liveness ping not recorded");
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Health, Instant)>> {
        // Nothing done under the lock panics.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "liveness_tests.rs"]
mod tests;
