// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin status`, `history`, `outages` and `timeline`: reading a running
//! server's state and history.
//!
//! Each prints the API's document as it comes (`--json`) or a short table.
//! The token, when set, goes with every read: without one the server
//! answers as it does the public, and raw results need one with
//! `status:read`.

use std::fmt::Write as _;

use tocsin_core::config::{Duration, Id};
use tocsin_core::incident::Timestamp;
use tocsin_core::release::{Timeline, TimelineEvent};
use tocsin_core::state::ComponentStatus;
use tocsin_core::status::{CheckResults, OutageList, StatusDocument};
use url::form_urlencoded;

use crate::cli::{HistoryArgs, ServerArgs};
use crate::client::Client;
use crate::exit::Exit;
use crate::output::{Failure, Report};

/// `tocsin status`: exits 1 when anything is degraded or down, so a
/// script can branch on it without reading the document.
pub fn status(server: &ServerArgs) -> Result<Report, Failure> {
    let document: StatusDocument = Client::new(server)?.read("api/v1/status.json")?;
    let exit = if matches!(
        document.status,
        ComponentStatus::Degraded | ComponentStatus::Down
    ) {
        Exit::CheckFailed
    } else {
        Exit::Ok
    };
    Ok(Report {
        exit,
        json: to_json(&document),
        text: describe_status(&document),
    })
}

/// `tocsin history <check>`.
pub fn history(server: &ServerArgs, check: &str, page: &HistoryArgs) -> Result<Report, Failure> {
    // It goes into the path: a `..` or `/` would address another route.
    Id::parse(check)
        .map_err(|error| Failure::new(Exit::InvalidInput, format!("check {check:?}: {error}")))?;
    let path = with_query(&format!("api/v1/checks/{check}/results"), &page.pairs());
    let results: CheckResults = Client::new(server)?.read(&path)?;
    Ok(Report {
        exit: Exit::Ok,
        json: to_json(&results),
        text: describe_results(&results),
    })
}

/// `tocsin outages`.
pub fn outages(server: &ServerArgs, page: &HistoryArgs) -> Result<Report, Failure> {
    let path = with_query("api/v1/outages", &page.pairs());
    let list: OutageList = Client::new(server)?.read(&path)?;
    Ok(Report {
        exit: Exit::Ok,
        json: to_json(&list),
        text: describe_outages(&list),
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
    let timeline: Timeline = Client::new(server)?.read(&with_query("api/v1/timeline", &pairs))?;
    Ok(Report {
        exit: Exit::Ok,
        json: to_json(&timeline),
        text: describe_timeline(&timeline),
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

fn to_json(value: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("API types serialize")
}

/// The overall status, a line per component, then the open incidents.
fn describe_status(document: &StatusDocument) -> String {
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
fn describe_results(results: &CheckResults) -> String {
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
fn describe_outages(list: &OutageList) -> String {
    if list.outages.is_empty() {
        return format!("no outages since {}", list.since);
    }
    let mut text = String::new();
    for outage in &list.outages {
        let ended = outage
            .ended_at
            .map_or_else(|| "open".to_owned(), |at| at.to_string());
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
fn describe_timeline(timeline: &Timeline) -> String {
    if timeline.events.is_empty() {
        return format!("nothing since {}", timeline.since);
    }
    let mut text = String::new();
    for event in &timeline.events {
        let line = match event {
            TimelineEvent::Outage {
                check_id,
                health,
                ended_at,
                ..
            } => {
                let ended =
                    ended_at.map_or_else(|| "still open".to_owned(), |at| format!("until {at}"));
                format!("outage    {check_id} {health}, {ended}")
            }
            TimelineEvent::Incident {
                incident_kind,
                title,
                status,
                ..
            } => format!("{incident_kind:<9} {title} ({status})"),
            TimelineEvent::Deploy { release, .. } => format!(
                "deploy    {} {} to {}",
                release.project, release.version, release.environment
            ),
            TimelineEvent::Release { release, .. } => {
                format!("release   {} {}", release.project, release.version)
            }
        };
        let _ = writeln!(text, "{}  {line}", event.at());
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
