// SPDX-License-Identifier: AGPL-3.0-only

//! `/llms.txt`: the current status in plain text, and where an agent goes
//! next: the OpenAPI document, the docs, and how to query history.
//!
//! It follows the llms.txt layout (a title, a one-line summary, then
//! sections of links) and shows only what `/api/v1/status.json` shows
//! the public.

use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use tocsin_core::incident::Timestamp;

use super::error::ApiError;
use super::timeline::DAY_MS;
use super::{AppState, OPENAPI_PATH, status};

/// Where it is served.
pub const PATH: &str = "/llms.txt";

/// The API reference, for readers that cannot fetch the OpenAPI document.
pub const DOCS_URL: &str = "https://github.com/closient/tocsin/blob/main/docs/api.md";

/// Serves the page.
///
/// # Errors
///
/// 500 if the store fails.
pub async fn get_llms_txt(State(state): State<AppState>) -> Result<Response, ApiError> {
    let text = render(&state).await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        text,
    )
        .into_response())
}

/// The page's text.
async fn render(state: &AppState) -> Result<String, ApiError> {
    let document = status::document(state).await?;
    let base = state
        .config
        .server
        .public_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    // A week back: inside every window, and not a date that ages out.
    let example = Timestamp::from_millis(document.generated_at.as_millis() - 7 * DAY_MS);
    let mut text = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(text, "# {}\n", document.title);
    let _ = writeln!(
        text,
        "> Status page for the services below, served by tocsin. Overall status: {} as of {}.\n",
        document.status, document.generated_at
    );
    text.push_str("## Components\n\n");
    if document.components.is_empty() {
        text.push_str("- none configured\n");
    }
    for component in &document.components {
        let _ = writeln!(
            text,
            "- [{}]({base}/api/v1/components/{}): {}",
            component.name, component.id, component.status
        );
    }
    text.push_str("\n## Open incidents and maintenance\n\n");
    if document.incidents.is_empty() {
        text.push_str("- none\n");
    }
    for incident in &document.incidents {
        let components = if incident.components.is_empty() {
            String::new()
        } else {
            format!(", affecting {}", incident.components.join(", "))
        };
        let _ = writeln!(
            text,
            "- [{}]({base}/api/v1/incidents/{}): {} {}, impact {}{components}",
            incident.title, incident.id, incident.kind, incident.status, incident.impact
        );
    }
    let _ = write!(
        text,
        "
## API

- [OpenAPI 3.1 document]({base}{OPENAPI_PATH}): every endpoint, parameter and schema
- [Current status]({base}/api/v1/status.json): this page as JSON
- [Checks]({base}/api/v1/checks): every check and heartbeat with its current health
- [API reference]({DOCS_URL}): endpoints, tokens and the `tocsin` CLI

## History

Times are RFC 3339. `since` defaults as noted and reaches back at most 366 days; `limit` is 1 to 500, default 100. A page with more after it carries `next_before` (`next_after` on the timeline): pass it back with the same `since`.

- [Outages]({base}/api/v1/outages?since={example}): outages open at any time since `since` (default 30 days ago), newest first
- [Timeline]({base}/api/v1/timeline?component=ID&since={example}): outages, incidents and deploys, oldest first (default 30 days)
- [Incidents]({base}/api/v1/incidents): incidents and maintenance, newest first, paged with `before`
- [Releases]({base}/api/v1/releases): the release log
- [Check results]({base}/api/v1/checks/ID/results?since={example}): one check's raw results, newest first (default one day); needs `Authorization: Bearer <token>` with the `status:read` scope
"
    );
    Ok(text)
}
