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

use super::{AppState, heartbeats, incidents, releases, timeline};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "tocsin",
        description = "Uptime monitor, status page and release log. Reads are public; \
                       writes need a `[[token]]` with the route's scope, and a \
                       `status:read` token also sees non-public projects.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only"),
        version = "1",
    ),
    tags(
        (name = incidents::TAG, description = "Incidents and scheduled maintenance"),
        (name = releases::TAG, description = "The release log and deploy stamps"),
        (name = timeline::TAG, description = "Outages, incidents and deploys in one stream"),
        (name = heartbeats::TAG, description = "Pings from scheduled jobs"),
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
        .merge(incidents::router())
        .merge(releases::router())
        .merge(timeline::router())
        .merge(heartbeats::router())
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
