// SPDX-License-Identifier: AGPL-3.0-only

//! The internal board: every check and heartbeat at `/internal/`, whether
//! or not a public component shows it, and one check's recent history at
//! `/internal/checks/{id}`. Read-only; configuration has no UI.
//!
//! - **Who sees it**: a signed-in operator ([`Operator`]), proven the way
//!   the admin pages prove one. Every refusal is a 403, auth off included.
//! - **Reads**: the live snapshot as a trusted reader has it
//!   ([`status::checks`]), the newest results of each kind
//!   ([`tocsin_store::Store::newest_results`]), half-hour latency slices for
//!   the sparklines, the daily rollups for the 90-day bars, and for one
//!   check its results, outages and its projects' releases.
//! - **Live updates**: the page loads htmx and its SSE extension, as `/`
//!   does, and re-fetches its live part when its own stream,
//!   `/internal/events`, names a check change, and every 60 seconds
//!   besides. That stream is the API's as a trusted reader sees it: the
//!   public `/api/v1/events` leaves out checks on no component. Without
//!   JavaScript the page is complete as served.
//! - **Headers**: `Cache-Control: private, no-store` and
//!   `X-Robots-Tag: noindex` on every response, refusals included. Nothing
//!   here appears in a feed, `/llms.txt` or the API description.

use std::collections::HashMap;
use std::fmt::Write as _;

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use jiff::tz::TimeZone;
use serde_json::Value;
use tocsin_core::Health;
use tocsin_core::config::{Config, FailureState, Link};
use tocsin_core::incident::Timestamp;
use tocsin_core::probe::AssertionVerdict;
use tocsin_core::status::{CheckKind, CheckResult, CheckState};
use tocsin_engine::MISSED_PING;
use tocsin_store::{
    Checks, ComponentScope, HistoryPage, Newest, Order, Projects, ResultFilter, ResultSlice,
    TimelineQuery,
};

use super::identity::Operator;
use super::page::{self, HtmlError, Layout};
use super::releases::Stamp;
use super::status::{Chrome, StatusLabel, ordered, tallies, today, zone};
use super::timeline::{Row, Rows, row};
use super::uptime::{self, Bar, Tallies};
use crate::http::auth::Viewer;
use crate::http::{AppState, now, status};

/// The event stream the open page listens to: the board's own, since
/// `/api/v1/events` leaves out every check on no component.
const EVENTS: &str = "/internal/events";

/// The sparkline's window: the last 24 hours, in half-hour slices.
const SPARK_SLICES: i64 = 48;
const SLICE_MS: i64 = 30 * 60 * 1000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The sparkline's drawing box: ten units a slice, the line above the
/// failure marks.
const SPARK_WIDTH: i64 = 480;
const LINE_TOP: f64 = 4.0;
const LINE_BOTTOM: f64 = 38.0;
const MARK_TOP: i64 = 42;
const MARK_HEIGHT: i64 = 6;

/// How far back the detail page reads outages and releases.
const TIMELINE_MS: i64 = 90 * DAY_MS;
/// At most this many timeline events on the detail page.
const TIMELINE_EVENTS: u32 = 100;
/// At most this many recent results on the detail page.
const RECENT_RESULTS: u32 = 50;
/// The longest expected or actual value shown before it is cut.
const VALUE_CHARS: usize = 200;

/// The routes. Unlike `/admin/` they exist whether or not auth is
/// configured: without it every request is refused with a 403.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/internal/", get(board))
        .route("/internal/fragments/board", get(board_fragment))
        .route("/internal/checks/{id}", get(check))
        .route("/internal/checks/{id}/live", get(check_fragment))
        .route(EVENTS, get(events))
        .layer(axum::middleware::map_response(private))
}

/// Keeps every response, a refusal included, out of shared caches and
/// search indexes. The event stream keeps `no-transform`, so no proxy
/// holds its events back to compress them.
async fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    let streamed = headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("no-transform"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if streamed {
            "private, no-store, no-transform"
        } else {
            "private, no-store"
        }),
    );
    headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    response
}

/// One check or heartbeat as the board shows it.
struct Card {
    href: String,
    name: String,
    status: StatusLabel,
    /// `HTTP check`, `heartbeat`.
    kind: String,
    interval: String,
    /// What a check probes; heartbeats have none.
    target: Option<String>,
    /// The public component it belongs to: its name and page.
    component: Option<(String, String)>,
    /// How long it has held its status, and since when.
    held: Option<(String, Stamp)>,
    last_run: Option<Stamp>,
    /// Checks only: heartbeats have no latency.
    spark: Option<Spark>,
    bar: Bar,
    /// The newest degraded or down result in the last 24 hours.
    problem: Option<Problem>,
    /// The newest result's assertions in a sentence.
    verdict: Option<String>,
    /// Heartbeats only.
    pulse: Option<Pulse>,
    links: Vec<Link>,
}

/// A recent failure.
struct Problem {
    at: Stamp,
    health: StatusLabel,
    text: String,
}

/// A heartbeat's pings.
struct Pulse {
    last_ping: Option<Stamp>,
    /// When it goes down without another ping.
    deadline: Option<Stamp>,
    overdue: bool,
    message: Option<String>,
}

/// A latency sparkline as SVG shapes, with its text equivalent.
struct Spark {
    /// One `points` list per unbroken run of slices with a latency.
    lines: Vec<String>,
    /// A failure mark under each slice with a degraded or down result.
    marks: Vec<Mark>,
    summary: String,
}

struct Mark {
    x: i64,
    width: i64,
    y: i64,
    height: i64,
    /// `down` or `degraded`.
    class: &'static str,
}

/// A group of cards: a `[[group]]`, the components in none, or the checks
/// on no component.
struct Section {
    name: String,
    description: Option<String>,
    cards: Vec<Card>,
}

/// The counts at the top of the board.
struct Summary {
    class: &'static str,
    glyph: &'static str,
    text: String,
}

/// One assertion's verdict, for the detail page.
struct VerdictRow {
    passed: bool,
    condition: String,
    expected: String,
    actual: String,
    /// What a failure does: `down` or `degraded`.
    on_fail: &'static str,
    implicit: bool,
    message: Option<String>,
}

/// One raw result, for the detail page.
struct ResultRow {
    at: Stamp,
    health: StatusLabel,
    duration: String,
    node: String,
    status_code: Option<u16>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "internal/board.html")]
struct BoardPage {
    layout: Layout,
    chrome: Chrome,
    events: &'static str,
    operator: String,
    summary: Summary,
    sections: Vec<Section>,
}

#[derive(Template)]
#[template(path = "internal/fragments/board.html")]
struct BoardFragment {
    summary: Summary,
    sections: Vec<Section>,
}

#[derive(Template)]
#[template(path = "internal/check.html")]
struct CheckPage {
    layout: Layout,
    chrome: Chrome,
    events: &'static str,
    live_url: String,
    detail: Detail,
}

#[derive(Template)]
#[template(path = "internal/fragments/check.html")]
struct CheckFragment {
    detail: Detail,
}

/// What the detail page shows below its heading.
struct Detail {
    card: Card,
    /// When the verdicts were reached.
    verdicts_at: Option<Stamp>,
    verdicts: Vec<VerdictRow>,
    results: Vec<ResultRow>,
    timeline: Vec<Row>,
    since: Stamp,
    truncated: bool,
}

async fn board(State(state): State<AppState>, operator: Operator) -> Result<Response, HtmlError> {
    let config = &state.config;
    let (summary, sections) = sections(&state).await?;
    let page = BoardPage {
        layout: Layout::public(format!("Internal board · {}", config.page.title))
            .themed(&config.page),
        chrome: Chrome::private(config).live(),
        events: EVENTS,
        operator: operator.email,
        summary,
        sections,
    };
    Ok(page::render(StatusCode::OK, &page))
}

/// Every event, every check's included, for the board and check pages
/// to refresh on.
async fn events(State(state): State<AppState>, _operator: Operator) -> Response {
    crate::http::events::stream(&state, Viewer::Trusted)
}

async fn board_fragment(
    State(state): State<AppState>,
    _operator: Operator,
) -> Result<Response, HtmlError> {
    let (summary, sections) = sections(&state).await?;
    Ok(page::render(
        StatusCode::OK,
        &BoardFragment { summary, sections },
    ))
}

async fn check(
    State(state): State<AppState>,
    _operator: Operator,
    Path(id): Path<String>,
) -> Result<Response, HtmlError> {
    let detail = detail(&state, &id).await?;
    let config = &state.config;
    let page = CheckPage {
        layout: Layout::public(format!("{} · Internal board", detail.card.name))
            .themed(&config.page),
        chrome: Chrome::private(config).live(),
        events: EVENTS,
        live_url: format!("/internal/checks/{id}/live"),
        detail,
    };
    Ok(page::render(StatusCode::OK, &page))
}

async fn check_fragment(
    State(state): State<AppState>,
    _operator: Operator,
    Path(id): Path<String>,
) -> Result<Response, HtmlError> {
    let detail = detail(&state, &id).await?;
    Ok(page::render(StatusCode::OK, &CheckFragment { detail }))
}

/// The board: every check and heartbeat under its component's group,
/// then those on no component, with the counts above.
async fn sections(state: &AppState) -> Result<(Summary, Vec<Section>), HtmlError> {
    let config = &state.config;
    let members = status::checks(config, &state.live.snapshot(), Viewer::Trusted);
    let mut cards = cards(state, &members).await?;
    let summary = summary(&members);
    let mut sections = Vec::new();
    for (group, components) in ordered(config) {
        let mut section = Section {
            name: group.map_or_else(
                || "Components in no group".to_owned(),
                |group| group.name.clone(),
            ),
            description: group.and_then(|group| group.description.clone()),
            cards: Vec::new(),
        };
        for component in components {
            for (member, card) in members.iter().zip(cards.iter_mut()) {
                if member.component.as_deref() == Some(component.id.as_str()) {
                    section.cards.extend(card.take());
                }
            }
        }
        sections.push(section);
    }
    sections.push(Section {
        name: "Not on the status page".to_owned(),
        description: Some("Checks and heartbeats that belong to no component.".to_owned()),
        cards: cards.into_iter().flatten().collect(),
    });
    sections.retain(|section| !section.cards.is_empty());
    Ok((summary, sections))
}

/// How many checks are in each state, coloured by the worst.
fn summary(members: &[CheckState]) -> Summary {
    let count = |health: Option<Health>| {
        members
            .iter()
            .filter(|member| member.health == health)
            .count()
    };
    let (up, degraded, down, unknown) = (
        count(Some(Health::Up)),
        count(Some(Health::Degraded)),
        count(Some(Health::Down)),
        count(None),
    );
    let worst = if down > 0 {
        Some(Health::Down)
    } else if degraded > 0 {
        Some(Health::Degraded)
    } else if unknown > 0 || members.is_empty() {
        None
    } else {
        Some(Health::Up)
    };
    let label = StatusLabel::check(worst);
    let mut text = format!("{up} up");
    for (number, word) in [(degraded, "degraded"), (down, "down"), (unknown, "unknown")] {
        if number > 0 {
            let _ = write!(text, " · {number} {word}");
        }
    }
    Summary {
        class: label.class,
        glyph: label.glyph,
        text,
    }
}

/// A card for each of `members`, in the same order; `Some` so a section
/// can take its own and leave the rest.
async fn cards(state: &AppState, members: &[CheckState]) -> Result<Vec<Option<Card>>, HtmlError> {
    let config = &state.config;
    let zone = zone(config);
    let now = now().as_millis();
    let day_ago = Timestamp::from_millis(now - DAY_MS);
    let newest = newest(state, members, day_ago).await?;
    let spark_start = spark_start(now);
    // A check's page reads its own results, not every check's day.
    let only = match members {
        [member] => Some(member.id.clone()),
        _ => None,
    };
    let slices = state
        .store
        .result_slices(Timestamp::from_millis(spark_start), SLICE_MS, only)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let mut by_check: HashMap<&str, Vec<&ResultSlice>> = HashMap::new();
    for slice in &slices {
        by_check.entry(&slice.check_id).or_default().push(slice);
    }
    let today = today();
    let tallies = tallies(state, today).await?;
    Ok(members
        .iter()
        .zip(newest)
        .map(|(member, found)| {
            let slices = by_check
                .get(member.id.as_str())
                .map_or(&[][..], Vec::as_slice);
            Some(card(config, &zone, now, member, &found, slices, &tallies))
        })
        .collect())
}

/// Each member's newest results, by what the card needs.
struct Found {
    latest: Option<CheckResult>,
    problem: Option<CheckResult>,
    /// Heartbeats only: the newest ping, not a missed deadline.
    ping: Option<CheckResult>,
}

/// The newest result, newest failure in the last day, and for a
/// heartbeat the newest ping, of each of `members`, in one read.
async fn newest(
    state: &AppState,
    members: &[CheckState],
    day_ago: Timestamp,
) -> Result<Vec<Found>, HtmlError> {
    let epoch = Timestamp::from_millis(0);
    let mut queries = Vec::new();
    for member in members {
        queries.push(Newest {
            check_id: member.id.clone(),
            since: epoch,
            filter: ResultFilter::Any,
        });
        queries.push(Newest {
            check_id: member.id.clone(),
            since: day_ago,
            filter: ResultFilter::NotUp,
        });
        if member.kind == CheckKind::Heartbeat {
            queries.push(Newest {
                check_id: member.id.clone(),
                since: epoch,
                filter: ResultFilter::ErrorNotStartingWith(MISSED_PING.to_owned()),
            });
        }
    }
    let mut found = state
        .store
        .newest_results(queries)
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .into_iter();
    Ok(members
        .iter()
        .map(|member| Found {
            latest: found.next().flatten(),
            problem: found.next().flatten(),
            ping: if member.kind == CheckKind::Heartbeat {
                found.next().flatten()
            } else {
                None
            },
        })
        .collect())
}

/// The start of the sparkline's window: 48 half-hours ending with the one
/// now is in, so a slice's bounds do not move between refreshes.
const fn spark_start(now: i64) -> i64 {
    now - now.rem_euclid(SLICE_MS) - (SPARK_SLICES - 1) * SLICE_MS
}

fn card(
    config: &Config,
    zone: &TimeZone,
    now: i64,
    member: &CheckState,
    found: &Found,
    slices: &[&ResultSlice],
    tallies: &Tallies,
) -> Card {
    let id = member.id.as_str();
    let heartbeat = config
        .heartbeats
        .iter()
        .find(|heartbeat| heartbeat.id.as_str() == id);
    let links = match heartbeat {
        Some(heartbeat) => heartbeat.links.clone(),
        None => config
            .checks
            .iter()
            .find(|check| check.id.as_str() == id)
            .map(|check| check.links.clone())
            .unwrap_or_default(),
    };
    let component = member.component.as_ref().map(|component| {
        (
            super::status::component_name(config, component),
            format!("/components/{component}"),
        )
    });
    let pulse = heartbeat.map(|heartbeat| {
        // The engine resumes a deadline from the newest stored result, ping
        // or miss, while the snapshot only learns of results after start.
        let last = found
            .latest
            .as_ref()
            .map(|result| result.started_at)
            .max(member.last_result_at);
        let deadline = last.map(|last| {
            last.as_millis()
                + i64::try_from(heartbeat.interval.get().as_millis()).unwrap_or(i64::MAX / 4)
                + i64::try_from(heartbeat.grace.get().as_millis()).unwrap_or(i64::MAX / 4)
        });
        Pulse {
            last_ping: found
                .ping
                .as_ref()
                .map(|ping| Stamp::zoned(ping.started_at, zone)),
            deadline: deadline.map(|at| Stamp::zoned(Timestamp::from_millis(at), zone)),
            overdue: deadline.is_some_and(|at| at < now),
            message: found.ping.as_ref().and_then(ping_message),
        }
    });
    Card {
        href: format!("/internal/checks/{id}"),
        name: member.name.clone(),
        status: StatusLabel::check(member.health),
        kind: match (&member.kind, &member.check_type) {
            (CheckKind::Heartbeat, _) => "heartbeat".to_owned(),
            (CheckKind::Check, Some(check_type)) => {
                format!("{} check", check_type.to_ascii_uppercase())
            }
            (CheckKind::Check, None) => "check".to_owned(),
        },
        interval: member.interval.clone(),
        target: member.target.clone(),
        component,
        held: member
            .since
            .map(|since| (span(now - since.as_millis()), Stamp::zoned(since, zone))),
        last_run: member.last_result_at.map(|at| Stamp::zoned(at, zone)),
        spark: (member.kind == CheckKind::Check).then(|| spark(slices)),
        bar: uptime::bar(tallies, std::slice::from_ref(&member.id), today()),
        problem: found.problem.as_ref().map(|result| Problem {
            at: Stamp::zoned(result.started_at, zone),
            health: StatusLabel::check(Some(result.health)),
            text: problem_text(result),
        }),
        verdict: found.latest.as_ref().and_then(verdict_sentence),
        pulse,
        links,
    }
}

/// `3h 12m`: how long ago, to the minute.
fn span(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match minutes {
        0 => "under a minute".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// What went wrong with a result, in a line.
fn problem_text(result: &CheckResult) -> String {
    match (&result.error, result.status_code) {
        (Some(error), _) => error.clone(),
        (None, Some(code)) => format!("HTTP {code}"),
        (None, None) => verdicts(result)
            .iter()
            .find(|verdict| !verdict.passed)
            .map_or_else(
                || "No detail recorded.".to_owned(),
                |verdict| format!("{} failed", verdict.condition),
            ),
    }
}

/// A result's assertion verdicts; none for a heartbeat or an old result.
fn verdicts(result: &CheckResult) -> Vec<AssertionVerdict> {
    result
        .detail
        .as_ref()
        .and_then(|detail| serde_json::from_value(detail.clone()).ok())
        .unwrap_or_default()
}

/// The newest result's assertions in a sentence: which failed, or that
/// all passed.
fn verdict_sentence(result: &CheckResult) -> Option<String> {
    let verdicts = verdicts(result);
    if verdicts.is_empty() {
        return None;
    }
    let failed: Vec<String> = verdicts
        .iter()
        .filter(|verdict| !verdict.passed)
        .map(|verdict| {
            format!(
                "{}: expected {}, got {}",
                verdict.condition,
                value_text(&verdict.expected),
                value_text(&verdict.actual)
            )
        })
        .collect();
    Some(if failed.is_empty() {
        match verdicts.len() {
            1 => "Its assertion passed.".to_owned(),
            all => format!("All {all} assertions passed."),
        }
    } else {
        format!(
            "{} of {} assertions failed. {}",
            failed.len(),
            verdicts.len(),
            failed.join("; ")
        )
    })
}

/// A heartbeat ping's message.
fn ping_message(ping: &CheckResult) -> Option<String> {
    ping.detail
        .as_ref()
        .and_then(|detail| detail.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A configured or observed value: strings bare, the rest as JSON, cut to
/// [`VALUE_CHARS`].
fn value_text(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.chars().count() > VALUE_CHARS {
        let mut cut: String = text.chars().take(VALUE_CHARS).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

/// The last 24 hours of `slices` (one check's, in slice order) as a
/// sparkline: the mean latency of each half-hour, broken where a
/// half-hour has none, with a mark under each one that saw a failure.
fn spark(slices: &[&ResultSlice]) -> Spark {
    let slot = SPARK_WIDTH / SPARK_SLICES;
    let peak = slices
        .iter()
        .filter_map(|slice| slice.mean_ms)
        .fold(0.0_f64, f64::max);
    let height = LINE_BOTTOM - LINE_TOP;
    let y = |mean: f64| {
        if peak > 0.0 {
            LINE_BOTTOM - mean / peak * height
        } else {
            LINE_BOTTOM
        }
    };
    let mut lines = Vec::new();
    let mut run: Vec<(i64, f64)> = Vec::new();
    let mut marks = Vec::new();
    let mut previous = -1;
    let (mut weighted, mut counted) = (0.0, 0_u32);
    let (mut fastest, mut slowest): (Option<i64>, Option<i64>) = (None, None);
    let mut failing = 0;
    let mut flush = |run: &mut Vec<(i64, f64)>| {
        let points = match run.as_slice() {
            [] => return,
            // One point: a short level stroke, so it shows.
            [(x, y)] => format!("{},{y:.1} {},{y:.1}", x - 3, x + 3),
            points => points
                .iter()
                .map(|(x, y)| format!("{x},{y:.1}"))
                .collect::<Vec<_>>()
                .join(" "),
        };
        lines.push(points);
        run.clear();
    };
    for slice in slices {
        if !(0..SPARK_SLICES).contains(&slice.slice) {
            continue;
        }
        let x = slice.slice * slot + slot / 2;
        if let Some(mean) = slice.mean_ms {
            if slice.slice != previous + 1 {
                flush(&mut run);
            }
            run.push((x, y(mean)));
            previous = slice.slice;
            let up = slice.results - slice.down;
            weighted += mean * f64::from(up);
            counted += up;
        } else {
            flush(&mut run);
            previous = -1;
        }
        fastest = min_option(fastest, slice.min_ms);
        slowest = max_option(slowest, slice.max_ms);
        if slice.down > 0 || slice.degraded > 0 {
            failing += 1;
            marks.push(Mark {
                x: slice.slice * slot + 1,
                width: slot - 2,
                y: MARK_TOP,
                height: MARK_HEIGHT,
                class: if slice.down > 0 { "down" } else { "degraded" },
            });
        }
    }
    flush(&mut run);
    let summary = if slices.is_empty() {
        "No results in the last 24 hours.".to_owned()
    } else {
        let mut summary = match (counted, fastest, slowest) {
            (1.., Some(fastest), Some(slowest)) => format!(
                "Latency over the last 24 hours: mean {} ms, from {fastest} to {slowest} ms.",
                (weighted / f64::from(counted)).round()
            ),
            _ => "No latency in the last 24 hours: every result was down.".to_owned(),
        };
        let _ = write!(
            summary,
            " Failures in {failing} of {SPARK_SLICES} half-hours."
        );
        summary
    };
    Spark {
        lines,
        marks,
        summary,
    }
}

fn min_option(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_option(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The assertion verdicts `result` records, as rows.
fn verdict_rows(result: &CheckResult) -> Vec<VerdictRow> {
    verdicts(result)
        .into_iter()
        .map(|verdict| VerdictRow {
            passed: verdict.passed,
            condition: verdict.condition,
            expected: value_text(&verdict.expected),
            actual: value_text(&verdict.actual),
            on_fail: match verdict.on_fail {
                FailureState::Degraded => "degraded",
                FailureState::Down => "down",
            },
            implicit: verdict.implicit,
            message: verdict.message,
        })
        .collect()
}

/// One check's detail: its card, its newest verdicts, its last day of
/// results and 90 days of outages and releases.
async fn detail(state: &AppState, id: &str) -> Result<Detail, HtmlError> {
    let config = &state.config;
    if !status::knows_check(config, id) {
        return Err(HtmlError::not_found());
    }
    let member = status::checks(config, &state.live.snapshot(), Viewer::Trusted)
        .into_iter()
        .find(|member| member.id == id)
        .ok_or_else(HtmlError::not_found)?;
    let zone = zone(config);
    let mut card = cards(state, std::slice::from_ref(&member))
        .await?
        .pop()
        .flatten()
        .ok_or_else(HtmlError::not_found)?;
    let now = now().as_millis();
    let results = state
        .store
        .check_results(
            id,
            HistoryPage {
                since: Timestamp::from_millis(now - DAY_MS),
                before: None,
                limit: RECENT_RESULTS,
            },
        )
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let latest = state
        .store
        .newest_results(vec![Newest {
            check_id: id.to_owned(),
            since: Timestamp::from_millis(0),
            filter: ResultFilter::Any,
        }])
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .pop()
        .flatten();
    let since = Timestamp::from_millis(now - TIMELINE_MS);
    let projects = projects(config, member.component.as_deref());
    let timeline = state
        .store
        .timeline(TimelineQuery {
            visible: Projects::All,
            shown: Checks::All,
            since,
            after: None,
            order: Order::NewestFirst,
            limit: TIMELINE_EVENTS,
            // No component id: only this check's outages, and no incident
            // matches the empty id.
            component: Some(ComponentScope {
                id: String::new(),
                checks: vec![id.to_owned()],
                projects,
            }),
        })
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let rows = Rows {
        incidents: "/incidents",
        zone: &zone,
        config,
    };
    // The detail page lists the verdicts in full; the sentence is for the
    // board.
    card.verdict = None;
    Ok(Detail {
        verdicts_at: latest
            .as_ref()
            .map(|result| Stamp::zoned(result.started_at, &zone)),
        verdicts: latest.as_ref().map(verdict_rows).unwrap_or_default(),
        results: results
            .iter()
            .map(|result| ResultRow {
                at: Stamp::zoned(result.started_at, &zone),
                health: StatusLabel::check(Some(result.health)),
                duration: format!("{} ms", result.duration_ms),
                node: result.node_id.clone(),
                status_code: result.status_code,
                error: result.error.clone(),
            })
            .collect(),
        timeline: timeline
            .events
            .iter()
            .map(|event| row(event, &rows))
            .collect(),
        since: Stamp::zoned(since, &zone),
        truncated: timeline.truncated,
        card,
    })
}

/// The projects whose releases show on a check's timeline: those listing
/// its component, or every project for a check on none.
fn projects(config: &Config, component: Option<&str>) -> Vec<String> {
    config
        .projects
        .iter()
        .filter(|project| {
            component.is_none_or(|component| {
                project
                    .components
                    .iter()
                    .any(|listed| listed.as_str() == component)
            })
        })
        .map(|project| project.id.as_str().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(slice: i64, mean_ms: Option<f64>, down: u32) -> ResultSlice {
        ResultSlice {
            check_id: "api".to_owned(),
            slice,
            results: 2,
            degraded: 0,
            down,
            mean_ms,
            min_ms: mean_ms.map(|_| 100),
            max_ms: mean_ms.map(|_| 300),
        }
    }

    #[test]
    fn a_gap_breaks_the_line_and_a_failure_is_marked() {
        let slices = [
            slice(0, Some(100.0), 0),
            slice(1, Some(200.0), 0),
            slice(3, Some(200.0), 1),
        ];
        let spark = spark(&slices.iter().collect::<Vec<_>>());
        assert_eq!(spark.lines.len(), 2, "{:?}", spark.lines);
        assert_eq!(spark.lines[0], "5,21.0 15,4.0");
        // A lone point is drawn as a short stroke.
        assert_eq!(spark.lines[1], "32,4.0 38,4.0");
        assert_eq!(spark.marks.len(), 1);
        assert_eq!(spark.marks[0].class, "down");
        assert_eq!(
            spark.summary,
            "Latency over the last 24 hours: mean 160 ms, from 100 to 300 ms. Failures in 1 of 48 half-hours."
        );
    }

    #[test]
    fn spans_read_to_the_minute() {
        assert_eq!(span(30_000), "under a minute");
        assert_eq!(span(5 * 60_000), "5m");
        assert_eq!(span(192 * 60_000), "3h 12m");
        assert_eq!(span(26 * 3_600_000), "1d 2h");
    }
}
