// SPDX-License-Identifier: AGPL-3.0-only

//! `/metrics`: every metric, for Prometheus to scrape, in the Prometheus
//! text format 0.0.4. docs/metrics.md lists them, with alert rules.
//!
//! It needs a `status:read` token, as `/api/v1/config/status` does: the
//! series name every check, those of projects the status page keeps
//! private included, and the node and notifiers. Prometheus sends one with
//! a scrape config's `authorization` block.

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::auth::{Authorized, scope};
use super::error::ApiError;
use crate::metrics::slack_names;

/// Where it is served.
pub const PATH: &str = "/metrics";

/// The media type of the Prometheus text format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Serves the metrics as they are now.
///
/// # Errors
///
/// 401 without a token or with an unknown one, 403 without the
/// `status:read` scope; 500 if the database's size or its Slack
/// connections cannot be read.
pub async fn get_metrics(
    State(state): State<AppState>,
    _reader: Authorized<scope::StatusRead>,
) -> Result<Response, ApiError> {
    let db_size_bytes = state.store.size_on_disk().map_err(|error| {
        ApiError::internal(&format!("cannot read the database's size: {error}"))
    })?;
    let slack = if slack_names(&state.config).is_some() {
        Some(state.store.slack_census().await.map_err(|error| {
            ApiError::internal(&format!("cannot read the Slack connections: {error}"))
        })?)
    } else {
        None
    };
    let text = state.metrics.render(
        &state.live.snapshot(),
        &state.config,
        db_size_bytes,
        slack.as_ref(),
    );
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE))],
        text,
    )
        .into_response())
}
