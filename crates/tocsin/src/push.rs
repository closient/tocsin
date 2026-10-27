// SPDX-License-Identifier: AGPL-3.0-only

//! Pushing this node's own rows to the nodes in its `push_to`
//! (`docs/design/federation.md`).
//!
//! One task per upstream reads the rows past that upstream's cursor, signs
//! them as one batch and posts it. The cursor moves only to what the
//! receipt says the upstream committed, so a push lost on the way, or a
//! crash between the upstream's commit and the cursor's, sends the rows
//! again, and the upstream skips what it already has. A failed push is
//! retried after a wait that doubles to a minute; a batch too large for
//! the upstream is halved. With a backlog the next push goes at once,
//! otherwise every `push_interval`, empty if need be, so the push doubles
//! as this node's heartbeat.
//!
//! Rollups go by day. Maintenance rewrites the recent days' rollups every
//! pass, and retention rewrites the days it finalizes, so after each it
//! moves every cursor back to the first day it rewrote
//! ([`Upstreams::rewrote`]) and those days go again.
//!
//! The upstreams are read only at startup.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use tocsin_alert::{Backoff, Jitter};
use tocsin_core::config::Config;
use tocsin_core::federation::NodeKey;
use tocsin_core::federation::wire::{self, Batch, BatchReceipt};
use tocsin_core::incident::Timestamp;
use tocsin_store::{Day, PushCursor, Store, Unpushed};
use tokio::task::JoinSet;

use crate::exit::Exit;
use crate::metrics::{Metrics, PushMetrics};
use crate::net;
use crate::output::Failure;

/// Rows in a batch, until an upstream answers 413.
const BATCH_ROWS: u32 = 1000;

/// How long a push may take before it counts as failed.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a refusal's body is logged.
const REFUSAL_BYTES: usize = 512;

/// How the pushers pace themselves.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacing {
    /// Between pushes with nothing left behind: `push_interval`.
    pub interval: Duration,
    /// After failed pushes.
    pub backoff: Backoff,
    /// The rows in a batch, before any 413.
    pub batch_rows: u32,
    /// The longest one push may take.
    pub timeout: Duration,
}

impl Pacing {
    fn from_config(config: &Config) -> Pacing {
        Pacing {
            interval: config
                .federation
                .as_ref()
                .map_or(Duration::from_secs(15), |federation| {
                    federation.push_interval.get()
                }),
            backoff: Backoff {
                base: Duration::from_secs(1),
                cap: Duration::from_secs(60),
                jitter: Jitter::Full,
            },
            batch_rows: BATCH_ROWS,
            timeout: TIMEOUT,
        }
    }
}

/// Each node in this node's `push_to`, with the URL it takes pushes at, in
/// the configuration's order.
pub(crate) fn targets(config: &Config) -> Vec<(String, String)> {
    let Some(node_id) = config.server.node_id.as_deref() else {
        return Vec::new();
    };
    let Some(local) = config.nodes.iter().find(|node| node.id.as_str() == node_id) else {
        return Vec::new();
    };
    local
        .push_to
        .iter()
        .filter_map(|target| {
            let node = config.nodes.iter().find(|node| node.id == *target)?;
            Some((target.as_str().to_owned(), node.url.clone()?))
        })
        .collect()
}

/// Whether `next` pushes somewhere else, or at another pace, than `running`.
/// Both are read only at startup.
pub(crate) fn settings_changed(running: &Config, next: &Config) -> bool {
    let interval = |config: &Config| {
        config
            .federation
            .as_ref()
            .map(|federation| federation.push_interval)
    };
    targets(running) != targets(next) || interval(running) != interval(next)
}

/// The nodes this node pushes to, as startup read them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Upstreams {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    node: String,
    key: Option<Arc<NodeKey>>,
    client: Option<reqwest::Client>,
    pacing: Option<Pacing>,
    metrics: PushMetrics,
    upstreams: Vec<Arc<Upstream>>,
}

#[derive(Debug)]
struct Upstream {
    id: String,
    endpoint: String,
    /// The first day maintenance rewrote since the pusher last looked, so
    /// a push in flight does not move the cursor past it.
    rewritten: Mutex<Option<Day>>,
}

impl Upstream {
    fn take_rewritten(&self) -> Option<Day> {
        self.rewritten
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl Upstreams {
    /// The upstreams of `config`'s node, each with its series in
    /// `metrics` started at 0.
    ///
    /// # Errors
    ///
    /// The node pushes somewhere and `[server] node_key` is missing or not
    /// a node key, which validation already refuses.
    pub(crate) fn from_config(config: &Config, metrics: &Metrics) -> Result<Upstreams, Failure> {
        let targets = targets(config);
        if targets.is_empty() {
            return Ok(Upstreams::default());
        }
        let key = config
            .server
            .node_key
            .as_ref()
            .ok_or_else(|| "`[server] node_key` is missing".to_owned())
            .and_then(|secret| NodeKey::parse(secret.expose()))
            .map_err(|error| {
                Failure::new(
                    Exit::Runtime,
                    format!("cannot push to other nodes: {error}"),
                )
            })?;
        let pacing = Pacing::from_config(config);
        let client = net::http_client(pacing.timeout).build().map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!(
                    "cannot start pushing to other nodes: {}",
                    net::chain(&error)
                ),
            )
        })?;
        let upstreams = targets
            .into_iter()
            .map(|(id, url)| {
                metrics.push.pushing_to(&id);
                Arc::new(Upstream {
                    endpoint: format!("{}{}", url.trim_end_matches('/'), wire::BATCH_PATH),
                    id,
                    rewritten: Mutex::new(None),
                })
            })
            .collect();
        Ok(Upstreams {
            inner: Arc::new(Inner {
                node: config.server.node_id.clone().unwrap_or_default(),
                key: Some(Arc::new(key)),
                client: Some(client),
                pacing: Some(pacing),
                metrics: metrics.push.clone(),
                upstreams,
            }),
        })
    }

    /// The same upstreams, paced by `pacing` instead.
    #[cfg(test)]
    pub(crate) fn paced(self, pacing: Pacing) -> Upstreams {
        let inner = Arc::into_inner(self.inner).expect("no pusher has started");
        Upstreams {
            inner: Arc::new(Inner {
                pacing: Some(pacing),
                ..inner
            }),
        }
    }

    /// Starts one pusher per upstream.
    pub(crate) fn start(&self, store: &Arc<Store>) -> Pushers {
        let mut tasks = JoinSet::new();
        let (Some(key), Some(client), Some(pacing)) =
            (&self.inner.key, &self.inner.client, self.inner.pacing)
        else {
            return Pushers { tasks };
        };
        for upstream in &self.inner.upstreams {
            let pusher = Pusher {
                node: self.inner.node.clone(),
                key: Arc::clone(key),
                upstream: Arc::clone(upstream),
                store: Arc::clone(store),
                client: client.clone(),
                metrics: self.inner.metrics.clone(),
                pacing,
            };
            tasks.spawn(pusher.run());
        }
        Pushers { tasks }
    }

    /// Maintenance rewrote this node's rollups from `from` on: every
    /// upstream gets them again from there.
    pub(crate) async fn rewrote(&self, store: &Store, from: Option<Day>) {
        let Some(from) = from else { return };
        if self.inner.upstreams.is_empty() {
            return;
        }
        // The cursors first, so a pusher that sees the day has it stored.
        if let Err(error) = store.repush_rollups_from(from).await {
            tracing::error!(%error, %from, "cannot mark rewritten rollups to push again");
        }
        for upstream in &self.inner.upstreams {
            let mut rewritten = upstream
                .rewritten
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *rewritten = earliest(*rewritten, Some(from));
        }
    }

    /// Retention deleted `unpushed` of this node's results, by upstream,
    /// before those upstreams acknowledged them.
    pub(crate) fn expired(&self, unpushed: &BTreeMap<String, u64>) {
        for upstream in &self.inner.upstreams {
            let Some(&rows) = unpushed.get(&upstream.id) else {
                continue;
            };
            self.inner.metrics.expired(&upstream.id, rows);
            tracing::error!(
                upstream = %upstream.id,
                rows,
                "retention deleted results before the upstream received them"
            );
        }
    }
}

/// The running pushers.
#[derive(Debug)]
pub(crate) struct Pushers {
    tasks: JoinSet<()>,
}

impl Pushers {
    /// Stops every pusher. A push in flight is abandoned; its rows go
    /// again from the stored cursor at the next start.
    pub(crate) async fn stop(mut self) {
        self.tasks.shutdown().await;
    }
}

/// What a push left to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// More is waiting and the last push moved the cursor: push again now.
    Now,
    /// Wait `push_interval`.
    Later,
}

/// Why a push failed.
#[derive(Debug)]
enum PushError {
    Store(tocsin_store::Error),
    Unanswered(String),
    Refused { status: StatusCode, body: String },
    Receipt(String),
}

impl fmt::Display for PushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::Store(error) => write!(formatter, "{error}"),
            PushError::Unanswered(error) => write!(formatter, "no answer: {error}"),
            PushError::Refused { status, body } => {
                write!(formatter, "refused with {status}: {body}")
            }
            PushError::Receipt(error) => write!(formatter, "the receipt is unreadable: {error}"),
        }
    }
}

impl From<tocsin_store::Error> for PushError {
    fn from(error: tocsin_store::Error) -> Self {
        PushError::Store(error)
    }
}

impl PushError {
    fn too_large(&self) -> bool {
        matches!(self, PushError::Refused { status, .. } if *status == StatusCode::PAYLOAD_TOO_LARGE)
    }
}

/// Pushes to one upstream until stopped.
struct Pusher {
    node: String,
    key: Arc<NodeKey>,
    upstream: Arc<Upstream>,
    store: Arc<Store>,
    client: reqwest::Client,
    metrics: PushMetrics,
    pacing: Pacing,
}

impl Pusher {
    async fn run(self) {
        let upstream = self.upstream.id.as_str();
        let mut batch_rows = self.pacing.batch_rows;
        let mut failures = 0;
        loop {
            let wait = match self.push(batch_rows).await {
                Ok(next) => {
                    if failures > 0 {
                        tracing::info!(upstream, failures, "pushing succeeds again");
                    }
                    failures = 0;
                    match next {
                        Next::Now => Duration::ZERO,
                        Next::Later => {
                            batch_rows = self.pacing.batch_rows;
                            self.pacing.interval
                        }
                    }
                }
                Err(error) => {
                    self.metrics.failed(upstream);
                    failures += 1;
                    if error.too_large() && batch_rows > 1 {
                        batch_rows = (batch_rows / 2).max(1);
                        tracing::warn!(upstream, batch_rows, "batch too large; halving it");
                        Duration::ZERO
                    } else {
                        let wait = self.pacing.backoff.wait(failures);
                        tracing::error!(upstream, failures, ?wait, %error, "push failed");
                        wait
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One push of at most `batch_rows` rows, and the cursor moved to
    /// what the upstream committed.
    async fn push(&self, batch_rows: u32) -> Result<Next, PushError> {
        let upstream = &self.upstream;
        // The stored cursor already holds whatever day was rewritten.
        upstream.take_rewritten();
        let store_id = self.store.store_id().await?;
        let cursor = self
            .store
            .push_cursor(upstream.id.clone(), self.node.clone())
            .await?;
        let unpushed = self
            .store
            .unpushed(self.node.clone(), cursor.clone(), batch_rows)
            .await?;
        let Unpushed {
            outages,
            results,
            rollups,
            rollups_next,
            head,
        } = unpushed;
        let sent_rollups = !rollups.is_empty();
        let batch = Batch {
            node: self.node.clone(),
            store: store_id.clone(),
            sent_at: Timestamp::from_millis(tocsin_store::now_ms()),
            config_digest: None,
            cursor: cursor.positions(),
            head,
            results,
            outages,
            rollups,
        };
        let body = serde_json::to_vec(&batch).expect("a batch is JSON");
        let signature = self.key.sign(&self.node, &store_id, &body);
        let response = self
            .client
            .post(&upstream.endpoint)
            .header(wire::NODE_HEADER, &self.node)
            .header(wire::STORE_HEADER, &store_id)
            .header(wire::SIGNATURE_HEADER, signature)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| PushError::Unanswered(net::chain(&error)))?;
        let status = response.status();
        if !status.is_success() {
            let mut body = response.text().await.unwrap_or_default();
            if body.len() > REFUSAL_BYTES {
                let end = body.floor_char_boundary(REFUSAL_BYTES);
                body.truncate(end);
                body.push('…');
            }
            return Err(PushError::Refused { status, body });
        }
        let receipt: BatchReceipt = response
            .json()
            .await
            .map_err(|error| PushError::Receipt(net::chain(&error)))?;

        // The first day left out, the cursor's own when results filled the
        // batch, or none once every day has gone.
        let mut rollups_from = rollups_next;
        if sent_rollups && let Some(skewed_at) = receipt.skewed_at {
            // The upstream left out days its clock has not reached.
            rollups_from = earliest(rollups_from, Some(Day::containing(skewed_at.as_millis())));
        }
        let next = PushCursor {
            results_through: receipt.cursor.results.unwrap_or(0),
            outages_through: receipt.cursor.outages.unwrap_or(0),
            rollups_from: earliest(rollups_from, upstream.take_rewritten()),
            acked_at_ms: Some(tocsin_store::now_ms()),
        };
        let moved =
            next.positions() != cursor.positions() || next.rollups_from != cursor.rollups_from;
        let behind = next.results_through < head.results.unwrap_or(0)
            || next.outages_through < head.outages.unwrap_or(0)
            || next.rollups_from.is_some();
        self.store
            .advance_push_cursor(upstream.id.clone(), next)
            .await?;
        Ok(if moved && behind {
            Next::Now
        } else {
            Next::Later
        })
    }
}

/// The earlier of two days, either of which may be missing.
fn earliest(first: Option<Day>, second: Option<Day>) -> Option<Day> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (day, None) | (None, day) => day,
    }
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;
