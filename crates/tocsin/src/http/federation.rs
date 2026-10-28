// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/federation`: pushes from the other nodes of a federation, and
//! what this node knows of each (`docs/design/federation.md`).
//!
//! A push is authenticated by its signature, never a token: the body is
//! signed with the pushing node's key, together with the node and store
//! it names, and verified against the public keys its `[[node]]` lists.
//! So every refusal before the signature checks out is the same 401, and
//! only a node that proves who it is learns that it is retired (403) or
//! conflicted (409). The receipt is sent only once the batch committed,
//! so a pusher that sees a 2xx can move its cursor, and only then does
//! the push ping the sender's `node:<id>` heartbeat ([`crate::liveness`]).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use tocsin_alert::Router;
use tocsin_core::Health;
use tocsin_core::config::{Id, Node};
use tocsin_core::federation::verify;
use tocsin_core::federation::wire::{
    self, Batch, BatchReceipt, LastPush, NodeList, NodeStatus, NodeStore, RemoteCheck,
};
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_engine::Ping;
use tocsin_store::{RemoteAlerting, RemoteBatch, RemoteOutageChange, RemoteStore, SubjectKind};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, scope};
use super::error::{self, ApiError, ErrorBody};
use super::{AppState, now};
use crate::liveness;
use crate::metrics::NodePush;
use crate::takeover::{AlertOrder, node_liveness};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "federation";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(push_batch))
        .routes(routes!(list_nodes))
}

/// Take a batch of results, outage changes and rollups another node
/// pushed from one of its stores.
#[utoipa::path(
    post,
    path = "/api/v1/federation/batch",
    tag = TAG,
    params(
        ("Tocsin-Node" = String, Header, description = "The pushing node's id, as its `[[node]]` names it"),
        ("Tocsin-Store" = String, Header, description = "The store the rows come from, 32 lowercase hex digits"),
        ("Tocsin-Signature" = String, Header, description = "The node key's signature over the node, the store and the body, base64"),
    ),
    request_body = Batch,
    responses(
        (status = 200, description = "Applied and committed", body = BatchReceipt),
        (status = 400, description = "The body is not JSON", body = ErrorBody),
        (status = 401, description = "A header is missing, the node is unknown, or the signature does not verify", body = ErrorBody),
        (status = 403, description = "The node, or the store it pushed from, is retired", body = ErrorBody),
        (status = 409, description = "The node is conflicted, this store was replaced by a newer one and pushed again, or the batch conflicts with what its store sent before, as two machines pushing as one node do", body = ErrorBody),
        (status = 413, description = "The body is too large", body = ErrorBody),
        (status = 422, description = "A row is another node's, out of order, or holds a bad value", body = ErrorBody),
    ),
)]
pub async fn push_batch(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<BatchReceipt>, ApiError> {
    // The headers and the node first, so a push no configured node could
    // have sent is refused without reading its body.
    let headers = request.headers();
    let node_id = header(headers, wire::NODE_HEADER)?.to_owned();
    let store = header(headers, wire::STORE_HEADER)?.to_owned();
    let signature = header(headers, wire::SIGNATURE_HEADER)?.to_owned();
    let node = state
        .config
        .nodes
        .iter()
        .find(|node| node.id.as_str() == node_id)
        .ok_or_else(refused)?;
    let body = error::body(request, &()).await?;
    if !verify(&node.public_keys, &node_id, &store, &body, &signature) {
        return Err(refused());
    }
    if node.retired {
        return Err(ApiError::forbidden(format!(
            "node {node_id} is retired: its pushes are refused"
        )));
    }
    let batch: Batch = error::parse(&body)?;
    let mut mismatched = Vec::new();
    if batch.node != node_id {
        mismatched.push(FieldError::new("node", "differs from Tocsin-Node"));
    }
    if batch.store != store {
        mismatched.push(FieldError::new("store", "differs from Tocsin-Store"));
    }
    if !mismatched.is_empty() {
        return Err(ApiError::invalid(mismatched));
    }
    let (sent_at, cursor, head, config_digest) = (
        batch.sent_at,
        batch.cursor,
        batch.head,
        batch.config_digest.clone(),
    );
    let remote_batch = RemoteBatch::from_wire(batch)?;
    let received_at = now();
    let local = state.config.server.node_id.clone().unwrap_or_default();
    let alerting = remote_alerting(&state, &remote_batch, received_at.as_millis());
    let applied = state
        .store
        .apply_batch(remote_batch, local, alerting, received_at.as_millis())
        .await;
    let report = match applied {
        Ok(report) => report,
        Err(conflict @ tocsin_store::Error::Conflict(_)) => {
            return Err(conflicted(&state, node, conflict).await);
        }
        Err(error) => return Err(error.into()),
    };
    for replaced in &report.replaced {
        tracing::warn!(
            node = %node_id,
            store = %store,
            replaced = %replaced,
            "a new store replaced the node's old one, as a rebuilt node's fresh database does: \
             the old store's open outages are withdrawn and its pushes are refused from now on"
        );
    }
    if report.alerts_queued > 0 {
        state.alert_wake.notify_one();
    }
    let receipt = report.receipt(cursor);
    state.metrics.federation.pushed(
        &node_id,
        NodePush {
            store,
            sent_at,
            received_at,
            backlog_rows: backlog(head, receipt.cursor),
            config_digest,
        },
    );
    if let Some(federation) = &state.config.federation {
        state
            .liveness
            .beat(
                &state.heartbeats,
                &state.metrics.checks,
                Id::synthetic(liveness::NODE, &node.id).as_str(),
                federation.push_interval.get(),
            )
            .await;
    }
    Ok(Json(receipt))
}

/// Two machines are pushing as `node`: its liveness goes down with the
/// reason, so it alerts now rather than when the refused pushes let it go
/// stale.
async fn conflicted(state: &AppState, node: &Node, conflict: tocsin_store::Error) -> ApiError {
    if let Some(federation) = &state.config.federation {
        let ping = Ping {
            health: Health::Down,
            message: Some(conflict.to_string()),
        };
        state
            .liveness
            .pulse(
                &state.heartbeats,
                &state.metrics.checks,
                Id::synthetic(liveness::NODE, &node.id).as_str(),
                ping,
                federation.push_interval.get(),
            )
            .await;
    }
    conflict.into()
}

/// How this node alerts for each outage `batch` opens that alerts, keyed by
/// its `origin_id` (docs/design/federation.md, "Who pages"). Empty unless
/// this node is in `alert_order`. A node opens alerts only while every node
/// ahead of it there is down: unheard, or its `node:<id>` heartbeat not
/// confirmed up. Behind one that is up, it re-fires only what it already
/// fired, until every node ahead goes down and it takes the outage over
/// (`crate::takeover`). A tocsin-kept heartbeat (`node:<id>`,
/// `upstream:<id>`) never pages from a push: each node watches those
/// itself.
fn remote_alerting(
    state: &AppState,
    batch: &RemoteBatch,
    received_at_ms: i64,
) -> BTreeMap<i64, RemoteAlerting> {
    let mut alerting = BTreeMap::new();
    let Some(order) = AlertOrder::of(&state.config) else {
        return alerting;
    };
    let opens_new = order.ahead_is_down(&state.live.snapshot(), Some(batch.node_id.as_str()));
    let mut router: Option<Router> = None;
    for change in &batch.outages {
        let RemoteOutageChange::Open {
            origin_id, outage, ..
        } = change
        else {
            continue;
        };
        if !outage.alerted || outage.check_id.contains(':') {
            continue;
        }
        let router = router.get_or_insert_with(|| Router::new(&state.config));
        alerting.insert(
            *origin_id,
            order.alerting(router, outage, opens_new, received_at_ms),
        );
    }
    alerting
}

/// Every node the configuration lists: whether it is retired, its stores
/// this node holds rows from, and what its last push here said.
#[utoipa::path(
    get,
    path = "/api/v1/federation/nodes",
    tag = TAG,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "In the configuration's order", body = NodeList),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `status:read` scope", body = ErrorBody),
    ),
)]
pub async fn list_nodes(
    State(state): State<AppState>,
    _reader: Authorized<scope::StatusRead>,
) -> Result<Json<NodeList>, ApiError> {
    Ok(Json(NodeList {
        nodes: nodes(&state).await?,
    }))
}

/// Every `[[node]]`, in configuration order, as this node sees it.
///
/// # Errors
///
/// A store read fails.
pub(crate) async fn nodes(state: &AppState) -> tocsin_store::Result<Vec<NodeStatus>> {
    let remote = state.store.remote_stores().await?;
    let local = state.config.server.node_id.as_deref().unwrap_or_default();
    let snapshot = state.live.snapshot();
    let mut nodes = Vec::with_capacity(state.config.nodes.len());
    for node in &state.config.nodes {
        let mut status = node_status(state, node, local, &remote);
        if !status.local {
            status.liveness = node_liveness(&snapshot, &node.id);
            status.checks = remote_checks(state, &status.id, status.liveness).await?;
        }
        nodes.push(status);
    }
    Ok(nodes)
}

/// Each configured check `node` has pushed rows for, with the state they
/// give it (docs/design/federation.md, "No stale up"): its open outage's
/// health, else `up` once it has an `up` result; nothing while the node's
/// `liveness` is not up, since a silent node's last word may be stale.
async fn remote_checks(
    state: &AppState,
    node: &str,
    liveness: Option<Health>,
) -> tocsin_store::Result<Vec<RemoteCheck>> {
    let ids: Vec<String> = state
        .config
        .checks
        .iter()
        .map(|check| check.id.as_str().to_owned())
        .collect();
    let open = state.store.open_outages(node).await?;
    let up = state.store.last_up(node, ids.clone()).await?;
    let latest = state.store.latest_results(node, ids.clone()).await?;
    let heard = liveness == Some(Health::Up);
    Ok(ids
        .into_iter()
        .filter_map(|id| {
            let outage = open
                .iter()
                .find(|outage| outage.subject_kind == SubjectKind::Check && outage.check_id == id);
            let last_up = up.iter().find(|up| up.check_id == id);
            let last_result_at = latest
                .iter()
                .find(|latest| latest.check_id == id)
                .map(|latest| Timestamp::from_millis(latest.started_at_ms));
            if outage.is_none() && last_result_at.is_none() {
                return None;
            }
            let confirmed = match (outage, last_up) {
                (Some(outage), _) => Some((outage.health, outage.started_at_ms)),
                (None, Some(last_up)) => Some((Health::Up, last_up.since_ms)),
                (None, None) => None,
            }
            .filter(|_| heard);
            Some(RemoteCheck {
                id,
                health: confirmed.map(|(health, _)| health),
                since: confirmed.map(|(_, since_ms)| Timestamp::from_millis(since_ms)),
                last_result_at,
            })
        })
        .collect())
}

/// What the configuration and this node's stores and metrics say of
/// `node`; its liveness and checks are left for [`nodes`] to fill in.
fn node_status(state: &AppState, node: &Node, local: &str, remote: &[RemoteStore]) -> NodeStatus {
    let id = node.id.as_str();
    NodeStatus {
        id: id.to_owned(),
        retired: node.retired,
        local: id == local,
        liveness: None,
        checks: Vec::new(),
        stores: remote
            .iter()
            .filter(|known| known.node_id == id)
            .map(|known| NodeStore {
                store: known.store_id.clone(),
                state: known.state,
                first_seen_at: Timestamp::from_millis(known.first_seen_at_ms),
                last_push_at: known.last_push_at_ms.map(Timestamp::from_millis),
            })
            .collect(),
        last_push: state.metrics.federation.last_push(id).map(|push| LastPush {
            clock_offset_seconds: push.clock_offset_seconds(),
            store: push.store,
            sent_at: push.sent_at,
            received_at: push.received_at,
            backlog_rows: push.backlog_rows,
            config_digest: push.config_digest,
        }),
    }
}

/// The rows a sender whose store ends at `head` has yet to push, once
/// this receiver is at `cursor`: none of a kind its head does not name.
pub(super) fn backlog(head: wire::Positions, cursor: wire::Positions) -> i64 {
    let behind = |head: Option<i64>, cursor: Option<i64>| {
        head.map_or(0, |head| (head - cursor.unwrap_or(0)).max(0))
    };
    behind(head.results, cursor.results) + behind(head.outages, cursor.outages)
}

/// The value of `name`, or the 401 every unauthenticated push gets.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ApiError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(refused)
}

/// The one answer to a push that does not prove which node sent it, so a
/// caller cannot tell an unknown node from a bad signature.
fn refused() -> ApiError {
    ApiError::unauthorized("the push is not signed by a configured node")
}
