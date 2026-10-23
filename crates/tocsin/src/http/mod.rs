// SPDX-License-Identifier: AGPL-3.0-only

//! The HTTP API.
//!
//! Extension points for later routes:
//! - **Routes**: add a module with an `OpenApiRouter<AppState>` and merge it
//!   in [`openapi::api`]; [`router`] serves everything registered there.
//! - **Auth**: take [`auth::Authorized<S>`] with a scope from
//!   [`auth::scope`]; the handler gets the [`Actor`](tocsin_core::incident::Actor).
//! - **Errors**: return [`error::ApiError`]; take bodies as
//!   [`error::ApiJson`] for 422s with field paths.
//! - **Events**: emit on [`AppState::events`] after a write commits.

pub mod auth;
pub mod error;
pub mod events;
pub mod incidents;
pub mod openapi;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tocsin_core::config::Config;
use tocsin_store::Store;

use self::error::ApiError;
use self::events::EventBus;

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    /// History: results, incidents.
    pub store: Arc<Store>,
    /// The configuration loaded at startup.
    pub config: Arc<Config>,
    /// Where writes announce what changed.
    pub events: EventBus,
}

impl AppState {
    /// Whether `[[component]]` `id` is configured.
    pub fn knows_component(&self, id: &str) -> bool {
        self.config
            .components
            .iter()
            .any(|component| component.id.as_str() == id)
    }
}

/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/v1/openapi.json";

/// The whole HTTP service.
pub fn router(state: AppState) -> Router {
    let (router, spec) = openapi::api().split_for_parts();
    let spec: Arc<str> = spec
        .to_pretty_json()
        .expect("the OpenAPI document serializes")
        .into();
    router
        .route(
            OPENAPI_PATH,
            get(move || async move {
                (
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    spec.to_string(),
                )
                    .into_response()
            }),
        )
        .fallback(|| async { ApiError::not_found("no such route") })
        .method_not_allowed_fallback(|| async { ApiError::method_not_allowed() })
        .with_state(state)
}
