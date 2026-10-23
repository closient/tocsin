// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin status`, `history`, `outages` and `timeline`: reading a running
//! server's state and history.
//!
//! Each prints the API's document as it came (`--json`), field for field,
//! or a short table read from it. The table reads leniently: every enum is
//! a string printed as it came and unknown fields are skipped, so a server
//! newer than this CLI still prints. The token, when set, goes with every
//! read: without one the server answers as it does the public, and raw
//! results need one with `status:read`.

use std::fmt::Write as _;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tocsin_core::config::{Duration, Id};
use tocsin_core::incident::Timestamp;
use url::form_urlencoded;

use crate::cli::{HistoryArgs, ServerArgs};
use crate::client::Client;
use crate::exit::Exit;
use crate::output::{Failure, Report};

/// `tocsin status`: exits 1 when anything is degraded or down, so a
/// script can branch on it without reading the document.
pub fn status(server: &ServerArgs) -> Result<Report, Failure> {
    let json = Client::new(server)?.read("api/v1/status.json")?;
    let document: StatusView = view(&json)?;
    let exit = if matches!(document.status.as_str(), "degraded" | "down") {
        Exit::CheckFailed
    } else {
        Exit::Ok
    };
    Ok(Report {
        exit,
        text: describe_status(&document),
        json,
    })
}

/// `tocsin history <check>`.
pub fn history(server: &ServerArgs, check: &str, page: &HistoryArgs) -> Result<Report, Failure> {
    // It goes into the path: a `..` or `/` would address another route.
    Id::parse(check)
        .map_err(|error| Failure::new(Exit::InvalidInput, format!("check {check:?}: {error}")))?;
    let path = with_query(&format!("api/v1/checks/{check}/results"), &page.pairs());
    let json = Client::new(server)?.read(&path)?;
    Ok(Report {
        exit: Exit::Ok,
        text: describe_results(&view(&json)?),
        json,
    })
}

/// `tocsin outages`.
pub fn outages(server: &ServerArgs, page: &HistoryArgs) -> Result<Report, Failure> {
    let path = with_query("api/v1/outages", &page.pairs());
    let json = Client::new(server)?.read(&path)?;
    Ok(Report {
        exit: Exit::Ok,
        text: describe_outages(&view(&json)?),
        json,
    })
}

/// `tocsin timeline`.
pub fn timeline(
    server: &ServerArgs,
    component: Option<&str>,
    since: Option<Timestamp>,
    after: Option<&str>,
    limit: Option<u32>,
) -> Result<Report, Failure> {
    let mut pairs = Vec::new();
    if let Some(component) = component {
        pairs.push(("component", component.to_owned()));
    }
    if let Some(since) = since {
        pairs.push(("since", since.to_string()));
    }
    if let Some(after) = after {
        pairs.push(("after", after.to_owned()));
    }
    if let Some(limit) = limit {
        pairs.push(("limit", limit.to_string()));
    }
    let json = Client::new(server)?.read(&with_query("api/v1/timeline", &pairs))?;
    Ok(Report {
        exit: Exit::Ok,
        text: describe_timeline(&view(&json)?),
        json,
    })
}

/// `--since`: a duration back from now, such as `2h` or `7d`, or an RFC
/// 3339 instant.
pub fn parse_since(text: &str) -> Result<Timestamp, String> {
    if let Ok(ago) = Duration::parse(text) {
        let ago = i64::try_from(ago.get().as_millis())
            .map_err(|_| format!("{text} reaches too far back"))?;
        return Ok(Timestamp::from_millis(
            crate::http::now().as_millis().saturating_sub(ago),
        ));
    }
    text.parse::<Timestamp>().map_err(|error| {
        format!("{error}; give a duration back from now such as 2h or 7d, or RFC 3339 such as 2026-10-23T14:05:00Z")
    })
}

impl HistoryArgs {
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(since) = self.since {
            pairs.push(("since", since.to_string()));
        }
        if let Some(before) = self.before {
            pairs.push(("before", before.to_string()));
        }
        if let Some(limit) = self.limit {
            pairs.push(("limit", limit.to_string()));
        }
        pairs
    }
}

/// `path?key=value&…`, each pair encoded; `path` alone without pairs.
fn with_query(path: &str, pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return path.to_owned();
    }
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!("{path}?{query}")
}

/// What the table needs of `json`; a document without it is not the
/// answer this CLI asked for.
fn view<T: DeserializeOwned>(json: &Value) -> Result<T, Failure> {
    T::deserialize(json).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("the server's answer is not what tocsin expects: {error}"),
        )
    })
}

/// `GET /api/v1/status.json`, as the table reads it.
#[derive(Deserialize)]
struct StatusView {
    title: String,
    status: String,
    generated_at: String,
    components: Vec<ComponentView>,
    incidents: Vec<IncidentView>,
}

#[derive(Deserialize)]
struct ComponentView {
    id: String,
    name: String,
    status: String,
}

#[derive(Deserialize)]
struct IncidentView {
    id: i64,
    kind: String,
    status: String,
    impact: String,
    title: String,
}

/// `GET /api/v1/checks/{id}/results`, as the table reads it.
#[derive(Deserialize)]
struct ResultsView {
    check_id: String,
    since: String,
    results: Vec<ResultView>,
    next_before: Option<i64>,
}

#[derive(Deserialize)]
struct ResultView {
    started_at: String,
    health: String,
    duration_ms: i64,
    error: Option<String>,
}

/// `GET /api/v1/outages`, as the table reads it.
#[derive(Deserialize)]
struct OutagesView {
    since: String,
    outages: Vec<OutageView>,
    next_before: Option<i64>,
}

#[derive(Deserialize)]
struct OutageView {
    started_at: String,
    ended_at: Option<String>,
    health: String,
    check_id: String,
    node_id: String,
}

/// `GET /api/v1/timeline`, as the table reads it.
#[derive(Deserialize)]
struct TimelineView {
    since: String,
    events: Vec<EventView>,
    next_after: Option<String>,
}

/// One event of any kind: each kind fills its own fields, and a kind this
/// CLI does not know prints by name.
#[derive(Deserialize)]
struct EventView {
    kind: String,
    at: String,
    check_id: Option<String>,
    health: Option<String>,
    ended_at: Option<String>,
    incident_kind: Option<String>,
    title: Option<String>,
    status: Option<String>,
    release: Option<ReleaseView>,
}

#[derive(Deserialize)]
struct ReleaseView {
    project: String,
    version: String,
    #[serde(default)]
    environment: String,
}

/// The overall status, a line per component, then the open incidents.
fn describe_status(document: &StatusView) -> String {
    let mut text = format!(
        "{}: {} as of {}\n",
        document.title, document.status, document.generated_at
    );
    for component in &document.components {
        let _ = writeln!(
            text,
            "  {:<11}  {:<20}  {}",
            component.status, component.id, component.name
        );
    }
    if !document.incidents.is_empty() {
        text.push_str("open:\n");
    }
    for incident in &document.incidents {
        let _ = writeln!(
            text,
            "{:>5}  {:<11}  {:<13}  {:<8}  {}",
            incident.id, incident.kind, incident.status, incident.impact, incident.title
        );
    }
    text.trim_end().to_owned()
}

/// One line per result.
fn describe_results(results: &ResultsView) -> String {
    if results.results.is_empty() {
        return format!(
            "no results for {} since {}",
            results.check_id, results.since
        );
    }
    let mut text = String::new();
    for result in &results.results {
        let _ = writeln!(
            text,
            "{}  {:<8}  {:>6}ms  {}",
            result.started_at,
            result.health,
            result.duration_ms,
            result.error.as_deref().unwrap_or("")
        );
    }
    if let Some(next) = results.next_before {
        let _ = writeln!(text, "more: --before {next}");
    }
    text.trim_end().to_owned()
}

/// One line per outage.
fn describe_outages(list: &OutagesView) -> String {
    if list.outages.is_empty() {
        return format!("no outages since {}", list.since);
    }
    let mut text = String::new();
    for outage in &list.outages {
        let ended = outage.ended_at.as_deref().unwrap_or("open");
        let _ = writeln!(
            text,
            "{}  {:<24}  {:<8}  {:<20}  {}",
            outage.started_at, ended, outage.health, outage.check_id, outage.node_id
        );
    }
    if let Some(next) = list.next_before {
        let _ = writeln!(text, "more: --before {next}");
    }
    text.trim_end().to_owned()
}

/// One line per event.
fn describe_timeline(timeline: &TimelineView) -> String {
    if timeline.events.is_empty() {
        return format!("nothing since {}", timeline.since);
    }
    let mut text = String::new();
    for event in &timeline.events {
        let field = |value: &Option<String>| value.clone().unwrap_or_default();
        let line = match (event.kind.as_str(), &event.release) {
            ("outage", _) => {
                let ended = event
                    .ended_at
                    .as_ref()
                    .map_or_else(|| "still open".to_owned(), |at| format!("until {at}"));
                format!(
                    "outage    {} {}, {ended}",
                    field(&event.check_id),
                    field(&event.health)
                )
            }
            ("incident", _) => format!(
                "{:<9} {} ({})",
                field(&event.incident_kind),
                field(&event.title),
                field(&event.status)
            ),
            ("deploy", Some(release)) => format!(
                "deploy    {} {} to {}",
                release.project, release.version, release.environment
            ),
            ("release", Some(release)) => {
                format!("release   {} {}", release.project, release.version)
            }
            (kind, _) => kind.to_owned(),
        };
        let _ = writeln!(text, "{}  {line}", event.at);
    }
    if let Some(next) = &timeline.next_after {
        let _ = writeln!(text, "more: --after {next}");
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_a_duration_back_or_an_instant() {
        let before = crate::http::now().as_millis();
        let ago = parse_since("2h").unwrap().as_millis();
        let after = crate::http::now().as_millis();
        assert!(
            (before - 7_200_000..=after - 7_200_000).contains(&ago),
            "{ago}"
        );
        assert_eq!(
            parse_since("2026-10-23T14:05:00Z").unwrap(),
            "2026-10-23T14:05:00Z".parse::<Timestamp>().unwrap()
        );
        let error = parse_since("yesterday").unwrap_err();
        assert!(error.contains("such as 2h or 7d"), "{error}");
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(with_query("api/v1/outages", &[]), "api/v1/outages");
        assert_eq!(
            with_query(
                "api/v1/timeline",
                &[("component", "a b&c".to_owned()), ("limit", "5".to_owned())]
            ),
            "api/v1/timeline?component=a+b%26c&limit=5"
        );
    }
}
