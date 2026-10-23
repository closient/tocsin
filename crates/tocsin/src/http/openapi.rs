// SPDX-License-Identifier: AGPL-3.0-only

//! The OpenAPI document, generated from the handlers.
//!
//! A new route group is registered by merging its `OpenApiRouter` in
//! [`api`]; its handlers' `#[utoipa::path]` attributes and the schemas they
//! name then appear in the document. `schema/openapi.json` is a committed
//! copy, checked by a test; regenerate it with `tocsin openapi`.

use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;

use super::{
    AppState, config_status, events, heartbeats, history, incidents, releases, status, subscribers,
    timeline,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "tocsin",
        description = "Uptime monitor, status page and release log. Reads are public; \
                       writes need a `[[token]]` with the route's scope, and a \
                       `status:read` token also sees non-public projects, check \
                       targets and raw check results.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only"),
        version = "1",
    ),
    tags(
        (name = status::TAG, description = "What is up right now: components and checks"),
        (name = history::TAG, description = "Outages and raw check results"),
        (name = incidents::TAG, description = "Incidents and scheduled maintenance"),
        (name = releases::TAG, description = "The release log and deploy stamps"),
        (name = timeline::TAG, description = "Outages, incidents and deploys in one stream"),
        (name = heartbeats::TAG, description = "Pings from scheduled jobs"),
        (name = config_status::TAG, description = "Whether the configuration reloaded"),
        (name = events::TAG, description = "Changes as they happen, as server-sent events"),
        (name = subscribers::TAG, description = "Email subscriptions to incidents and releases"),
    ),
    modifiers(&BearerAuth),
)]
struct Document;

/// Declares the `bearer` scheme that write routes require.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        let mut scheme = Http::new(HttpAuthScheme::Bearer);
        scheme.description = Some("A `[[token]]` secret from the configuration.".to_owned());
        components.add_security_scheme("bearer", SecurityScheme::Http(scheme));
    }
}

/// Every API route, with its documentation.
pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(Document::openapi())
        .merge(status::router())
        .merge(history::router())
        .merge(incidents::router())
        .merge(releases::router())
        .merge(timeline::router())
        .merge(heartbeats::router())
        .merge(config_status::router())
        .merge(events::router())
        .merge(subscribers::router())
}

/// The OpenAPI document.
pub fn spec() -> utoipa::openapi::OpenApi {
    api().split_for_parts().1
}

/// The document as committed in `schema/openapi.json`.
pub fn pretty() -> String {
    let mut text = spec()
        .to_pretty_json()
        .expect("the OpenAPI document serializes");
    text.push('\n');
    text
}
