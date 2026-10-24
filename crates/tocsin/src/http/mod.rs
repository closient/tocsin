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
pub mod config_status;
pub mod error;
pub mod events;
pub mod heartbeats;
pub mod history;
pub mod incidents;
pub mod llms;
pub mod metrics;
pub mod openapi;
pub mod public;
pub mod releases;
pub mod status;
pub mod subscribers;
pub mod timeline;

#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod release_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tocsin_core::config::Config;
use tocsin_core::incident::Timestamp;
use tocsin_engine::{HeartbeatHandle, Snapshot, StatusHandle};
use tocsin_store::Store;
use tokio::sync::watch;

pub use self::error::ApiError;
use self::events::EventBus;
use crate::metrics::Metrics;
use crate::reload::ConfigStatus;
use crate::subscribers::Subscriptions;
use crate::web::{self, identity::AdminAuth};

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    /// History: results, incidents, releases.
    pub store: Arc<Store>,
    /// The running configuration. A reload serves the requests that
    /// arrive after it from a new state; one under way keeps this one.
    pub config: Arc<Config>,
    /// Where writes announce what changed.
    pub events: EventBus,
    /// The admin pages' authentication; `None` leaves them off.
    pub admin: Option<Arc<AdminAuth>>,
    /// Takes heartbeat pings to the running engine.
    pub heartbeats: HeartbeatHandle,
    /// The checks' and components' current state.
    pub live: Arc<dyn LiveStatus>,
    /// How the configuration last reloaded.
    pub config_status: watch::Receiver<ConfigStatus>,
    /// Every metric, served at `/metrics`.
    pub metrics: Arc<Metrics>,
    /// Email subscriptions; `None` without `[subscribers]`, which leaves
    /// the form off the page and the subscriber API answering 404.
    pub subscribers: Option<Arc<Subscriptions>>,
}

/// Where the read API gets the current state of every check and
/// component: the running engine, or a fixed snapshot in tests.
pub trait LiveStatus: Send + Sync {
    /// The state now.
    fn snapshot(&self) -> Snapshot;
}

impl LiveStatus for StatusHandle {
    fn snapshot(&self) -> Snapshot {
        StatusHandle::snapshot(self)
    }
}

impl LiveStatus for Snapshot {
    fn snapshot(&self) -> Snapshot {
        self.clone()
    }
}

impl AppState {
    /// Whether `[[component]]` `id` is configured.
    pub fn knows_component(&self, id: &str) -> bool {
        self.config
            .components
            .iter()
            .any(|component| component.id.as_str() == id)
    }

    /// Whether `[[project]]` `id` is configured.
    pub fn knows_project(&self, id: &str) -> bool {
        self.config
            .projects
            .iter()
            .any(|project| project.id.as_str() == id)
    }

    /// Whether `[[project]]` `id` is configured and public.
    pub fn project_is_public(&self, id: &str) -> bool {
        self.config
            .projects
            .iter()
            .any(|project| project.id.as_str() == id && project.public)
    }
}

/// The current instant, as writes record it.
pub fn now() -> Timestamp {
    Timestamp::from_millis(tocsin_store::now_ms())
}

/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/v1/openapi.json";

/// The largest request body accepted; a larger one is 413.
pub const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// The whole HTTP service: the JSON API and the HTML pages.
pub fn router(state: AppState) -> Router {
    let (router, spec) = openapi::api().split_for_parts();
    let router = router.merge(web::router(&state));
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
        .route(llms::PATH, get(llms::get_llms_txt))
        .route(metrics::PATH, get(metrics::get_metrics))
        .merge(crate::mcp::http::router(&state))
        .fallback(|| async { ApiError::not_found("no such route") })
        .method_not_allowed_fallback(|| async { ApiError::method_not_allowed() })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}
