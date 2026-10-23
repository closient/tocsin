// SPDX-License-Identifier: AGPL-3.0-only

//! What every HTML page shares: the [`Layout`] `base.html` reads, the
//! [`render`] helper, error pages, and the security headers.

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use tocsin_core::incident::Timestamp;

use super::assets;
use super::identity::Admin;

/// The fields `templates/base.html` reads. Every page template extends
/// `base.html` and has a `layout: Layout` field.
pub struct Layout {
    /// The page's `<title>`, before " · tocsin".
    pub title: String,
    /// The stylesheet's URL, with its content hash.
    pub stylesheet: &'static str,
    /// The signed-in person, shown in the header.
    pub signed_in: Option<String>,
    /// The Atom feeds the page advertises to feed readers, most specific
    /// first.
    pub feeds: Vec<FeedLink>,
}

/// An Atom feed named in a page's `<head>` for feed readers to find.
pub struct FeedLink {
    pub title: String,
    pub href: String,
}

impl Layout {
    /// A public page: nobody signed in.
    pub fn public(title: impl Into<String>) -> Self {
        Layout {
            title: title.into(),
            stylesheet: assets::url("tocsin.css"),
            signed_in: None,
            feeds: Vec::new(),
        }
    }

    /// The same, advertising `feeds`.
    #[must_use]
    pub fn with_feeds(mut self, feeds: Vec<FeedLink>) -> Self {
        self.feeds = feeds;
        self
    }

    /// A page for a signed-in person, who is named in the header. Admin
    /// pages load no script: their forms are plain posts.
    pub fn admin(title: impl Into<String>, admin: &Admin) -> Self {
        Layout {
            title: title.into(),
            stylesheet: assets::url("tocsin.css"),
            signed_in: Some(admin.email.clone()),
            feeds: Vec::new(),
        }
    }
}

/// `at` for a person: `2026-10-23 14:05 UTC`. Pages put the exact instant
/// beside it in a `<time datetime>`.
pub fn when(at: Timestamp) -> String {
    jiff::Timestamp::from_millisecond(at.as_millis()).map_or_else(
        |_| at.to_string(),
        |instant| instant.strftime("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// `at` for a person in `zone`: `2026-10-23 16:05 CEST`, or `... UTC`.
/// The public pages use the `[page] timezone`.
pub fn when_in(at: Timestamp, zone: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(at.as_millis()).map_or_else(
        |_| at.to_string(),
        |instant| {
            instant
                .to_zoned(zone.clone())
                .strftime("%Y-%m-%d %H:%M %Z")
                .to_string()
        },
    )
}

/// `page` as an HTML response with `status`.
pub fn render(status: StatusCode, page: &impl Template) -> Response {
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(error) => HtmlError::internal(&error).into_response(),
    }
}

/// An error, shown as a page.
#[derive(Debug)]
pub struct HtmlError {
    status: StatusCode,
    message: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage<'a> {
    layout: Layout,
    message: &'a str,
}

impl HtmlError {
    /// An error page with `status` and a message for the person.
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        HtmlError {
            status,
            message: message.into(),
        }
    }

    /// 404.
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "There is no page here.")
    }

    /// 500. The cause is logged, not shown.
    pub fn internal(cause: &dyn std::fmt::Display) -> Self {
        tracing::error!(error = %cause, "page failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong; the server log says what.",
        )
    }
}

impl From<crate::http::error::ApiError> for HtmlError {
    fn from(error: crate::http::error::ApiError) -> Self {
        HtmlError::new(error.status(), error.message().to_owned())
    }
}

impl IntoResponse for HtmlError {
    fn into_response(self) -> Response {
        let page = ErrorPage {
            layout: Layout::public(self.status.canonical_reason().unwrap_or("Error")),
            message: &self.message,
        };
        match page.render() {
            Ok(html) => (self.status, Html(html)).into_response(),
            // The error page itself failed: plain text still says what.
            Err(_) => (self.status, self.message).into_response(),
        }
    }
}

/// The Content-Security-Policy on every page: only this site's own
/// stylesheet and script, no inline code, no framing.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; \
     frame-ancestors 'none'; base-uri 'none'";

/// Adds the security headers every HTML response gets, and `no-store`
/// unless the response set its own caching.
pub async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
