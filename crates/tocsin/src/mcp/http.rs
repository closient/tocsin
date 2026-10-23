// SPDX-License-Identifier: AGPL-3.0-only

//! `/mcp`: the tools over streamable HTTP, behind the API's bearer tokens.
//!
//! The endpoint is stateless. Every POST is answered in full with one JSON
//! response and no session or stream outlives it, so shutdown drains `/mcp`
//! as it drains any other route. A request without a known token is 401
//! before MCP sees it; each tool then checks the scope its REST route
//! needs.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::Server;
use crate::http::auth::Caller;
use crate::http::{AppState, BODY_LIMIT};

/// Where the MCP endpoint is served.
pub const PATH: &str = "/mcp";

/// `/mcp`, serving the tools over `state`.
pub fn router(state: &AppState) -> Router<AppState> {
    let tools = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(Server::local(tools.clone())),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            // The bearer token is what keeps other sites out, as on every
            // route; tocsin runs behind proxies under any host name.
            .disable_allowed_hosts()
            .with_max_request_body_bytes(BODY_LIMIT),
    );
    Router::new()
        .route_service(PATH, service)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
}

/// Admits a request whose bearer token tocsin knows, recording who sent
/// it for the tools; anything else is 401.
async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    match Caller::authenticate(&parts, &state) {
        Ok(caller) => {
            parts.extensions.insert(caller);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(error) => error.into_response(),
    }
}
