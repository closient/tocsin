// SPDX-License-Identifier: AGPL-3.0-only

//! Server-rendered HTML pages: the public status page, the public release
//! log, and the admin pages. `docs/admin.md` describes the pattern for
//! adding a page.
//!
//! - **Templates**: askama, in `crates/tocsin/templates/`. Every page
//!   extends `base.html` and carries a [`page::Layout`]; askama escapes
//!   every value, so only [`markdown::render`]'s sanitized output is marked
//!   `|safe`.
//! - **Assets**: [`assets`] compiles `crates/tocsin/static/` into the binary
//!   and serves each file under its content hash, cached as immutable.
//!   htmx is vendored there; no page loads anything from another origin.
//! - **Identity**: a page that needs a signed-in person takes
//!   [`identity::Admin`]; a write takes [`identity::AdminForm`], which adds
//!   the CSRF checks in [`csrf`].
//! - **Headers**: every response from these routes gets
//!   [`page::security_headers`]: a strict CSP, `nosniff`, no framing, and
//!   `no-store` unless it sets its own caching.

pub mod access;
pub mod admin;
pub mod assets;
pub mod csrf;
pub mod feeds;
pub mod form;
pub mod identity;
pub mod markdown;
pub mod page;
pub mod releases;
pub mod status;
pub mod timeline;
pub mod uptime;

#[cfg(test)]
mod feeds_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;

use axum::Router;
use axum::routing::get;

use crate::http::AppState;

/// The HTML routes: assets, the status page, the release log and their feeds always, the admin pages
/// when `[server.auth]` turns them on. Without it, `/admin/` is the same 404 as any unknown
/// path.
pub fn router(state: &AppState) -> Router<AppState> {
    let mut router = Router::new()
        .route(
            &format!("{}/{{hash}}/{{name}}", assets::PREFIX),
            get(assets::serve),
        )
        .merge(status::router())
        .merge(releases::router())
        .merge(feeds::router());
    if state.admin.is_some() {
        router = router.merge(admin::router()).merge(timeline::router());
    }
    router.layer(axum::middleware::map_response(page::security_headers))
}
