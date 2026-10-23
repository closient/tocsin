// SPDX-License-Identifier: AGPL-3.0-only

//! `[page] accent` as a stylesheet: `/theme/<hash>/accent.css` sets the
//! `--accent` token for the light and the dark scheme. The page's content
//! security policy allows stylesheets from this origin only and no inline
//! style, so the colour is served rather than written into the page.
//!
//! `<hash>` is the start of the body's SHA-256, so the response is cached
//! as immutable like the static assets; after a reload changes the accent,
//! pages link the new hash and the old one is a 404.
//!
//! Only `--accent` changes. `--accent-fg`, the text on an accent fill,
//! stays white in the light scheme and the page background in the dark:
//! contrast is symmetric, so an accent that reaches 4.5:1 on those
//! backgrounds (which loading the configuration checks) has text in them
//! at 4.5:1 on it too.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tocsin_core::config::Page;

use super::assets;
use crate::http::AppState;

/// Where the accent stylesheet is served, under its hash.
pub const PREFIX: &str = "/theme";

/// The stylesheet for `page`'s accent; `None` when neither scheme sets one.
pub fn css(page: &Page) -> Option<String> {
    let (light, dark) = page.accents();
    if light.is_none() && dark.is_none() {
        return None;
    }
    let light = light.map_or_else(String::new, |light| {
        format!(":root {{ --accent: {light}; }}\n")
    });
    let dark = dark.map_or_else(String::new, |dark| {
        format!("@media (prefers-color-scheme: dark) {{ :root {{ --accent: {dark}; }} }}\n")
    });
    let css = format!("/* [page] accent */\n{light}{dark}");
    Some(css)
}

fn hash(css: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, css.as_bytes());
    assets::hex(&digest.as_ref()[..8])
}

/// The URL of `page`'s accent stylesheet, if it has one.
pub fn url(page: &Page) -> Option<String> {
    css(page).map(|css| format!("{PREFIX}/{}/accent.css", hash(&css)))
}

/// `GET /theme/{hash}/accent.css`.
pub async fn serve(State(state): State<AppState>, Path(wanted): Path<String>) -> Response {
    match css(&state.config.page) {
        Some(css) if hash(&css) == wanted => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/css; charset=utf-8"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                ),
            ],
            css,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
