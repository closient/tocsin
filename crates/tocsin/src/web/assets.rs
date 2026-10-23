// SPDX-License-Identifier: AGPL-3.0-only

//! Static files, compiled into the binary and served at
//! `/static/<hash>/<name>`, where `<hash>` is the start of the file's
//! SHA-256. A changed file gets a new URL, so every response can be cached
//! for a year as immutable.
//!
//! To add a file: put it in `crates/tocsin/static/`, add it to [`FILES`],
//! and link to it with [`url`].

use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Where assets are served.
pub const PREFIX: &str = "/static";

/// Every asset: name, media type, contents.
const FILES: &[(&str, &str, &[u8])] = &[
    (
        "tocsin.css",
        "text/css; charset=utf-8",
        include_bytes!("../../static/tocsin.css"),
    ),
    (
        "htmx.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../static/htmx.min.js"),
    ),
];

struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
    hash: String,
    url: String,
}

static ASSETS: LazyLock<Vec<Asset>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|&(name, content_type, bytes)| {
            let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
            let hash = hex(&digest.as_ref()[..8]);
            let url = format!("{PREFIX}/{hash}/{name}");
            Asset {
                name,
                content_type,
                bytes,
                hash,
                url,
            }
        })
        .collect()
});

/// The URL of asset `name`, with its content hash.
///
/// # Panics
///
/// `name` is not in [`FILES`]: a typo in a page, caught by any test that
/// renders it.
pub fn url(name: &str) -> &'static str {
    ASSETS.iter().find(|asset| asset.name == name).map_or_else(
        || panic!("no static asset named {name:?}"),
        |asset| asset.url.as_str(),
    )
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 0xf)],
            ]
        })
        .map(char::from)
        .collect()
}

/// `GET /static/{hash}/{name}`. A stale hash is a 404, not the current
/// file: an immutable response must never change behind its URL.
pub async fn serve(Path((hash, name)): Path<(String, String)>) -> Response {
    let Some(asset) = ASSETS
        .iter()
        .find(|asset| asset.name == name && asset.hash == hash)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        asset.bytes,
    )
        .into_response()
}
