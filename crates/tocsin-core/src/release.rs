// SPDX-License-Identifier: AGPL-3.0-only

//! The release log: releases and deploys of configured `[[project]]`s, the
//! write requests, the rules every write must follow, and the timeline
//! that interleaves deploys with outages and incidents.
//!
//! A release is keyed by project, version and environment. CI publishes
//! it (title, notes, link) and stamps when it was deployed; either may come
//! first. Publishing the same key again updates the release in place.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::Health;
use crate::incident::{
    Actor, FieldError, Impact, IncidentKind, IncidentStatus, Timestamp, check_title, into_result,
};
use crate::link::http_url;

/// The environment a release or deploy is for when none is given.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// The longest version, in characters.
pub const MAX_VERSION_LENGTH: usize = 64;

/// The longest environment name, in characters.
pub const MAX_ENVIRONMENT_LENGTH: usize = 32;

/// The largest release notes, in bytes of UTF-8.
pub const MAX_NOTES_BYTES: usize = 64 * 1024;

/// The longest link, in bytes.
pub const MAX_URL_LENGTH: usize = 2048;

/// A release of a project to one environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Release {
    /// Assigned by tocsin; never reused.
    pub id: i64,
    /// The `[[project]]` id.
    pub project: String,
    /// The version, as CI named it.
    pub version: String,
    /// Where it was released, such as `production` or `staging`.
    pub environment: String,
    /// One line. Empty while the release is only a deploy stamp waiting
    /// for its publish.
    pub title: String,
    /// Markdown. Empty when none were given.
    pub notes: String,
    /// A link to the release, such as its tag or changelog.
    pub url: Option<String>,
    /// When it was published. `null` for a deploy stamped before its
    /// publish; never moved earlier once set.
    pub released_at: Option<Timestamp>,
    /// When it was last deployed, if it has been: the latest deploy time
    /// it has been given.
    pub deployed_at: Option<Timestamp>,
    /// When tocsin first heard of it.
    pub created_at: Timestamp,
    /// Who first wrote it.
    pub created_by: Actor,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// Who made the last change.
    pub updated_by: Actor,
}

impl Release {
    /// The title, or the version while the release has none.
    #[must_use]
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.version
        } else {
            &self.title
        }
    }
}

/// `POST /api/v1/releases`: publish a release, or update the one with the
/// same project, version and environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRelease {
    /// A configured `[[project]]` id.
    pub project: String,
    /// 1 to 64 characters: ASCII letters, digits and `.`, `_`, `+`, `-`,
    /// starting with a letter or digit, such as `v1.4.0` or `2026.10.23+5`.
    #[schema(max_length = 64, pattern = "^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$")]
    pub version: String,
    /// Defaults to `production`. 1 to 32 characters, the same set as
    /// `version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 32, pattern = "^[A-Za-z0-9][A-Za-z0-9._+-]{0,31}$")]
    pub environment: Option<String>,
    /// One line, at most 200 characters. Defaults to the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 200)]
    pub title: Option<String>,
    /// Markdown, at most 64 KiB. Defaults to none.
    #[serde(default)]
    pub notes: String,
    /// An `http` or `https` link to the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When it was released. Defaults to now on the first publish and to
    /// the recorded time on a re-publish; it may move later, never earlier,
    /// and never more than an hour past the server's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<Timestamp>,
    /// When it was deployed, if it already has been. Left as recorded when
    /// omitted or earlier than the recorded time; never more than an hour
    /// past the server's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployed_at: Option<Timestamp>,
}

impl NewRelease {
    /// The environment, with the default filled in.
    #[must_use]
    pub fn environment(&self) -> &str {
        self.environment.as_deref().unwrap_or(DEFAULT_ENVIRONMENT)
    }

    /// The title, with the default filled in.
    #[must_use]
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.version)
    }

    /// Checks every rule a release must follow. `known` says whether a
    /// project id is configured. Times are checked against the clock by
    /// the store, which owns the tolerance.
    ///
    /// # Errors
    ///
    /// [`Rejected::NotesTooLarge`] for notes over [`MAX_NOTES_BYTES`],
    /// before anything else is looked at; otherwise every problem found,
    /// each at its field path.
    pub fn validate(&self, known: &dyn Fn(&str) -> bool) -> Result<(), Rejected> {
        if self.notes.len() > MAX_NOTES_BYTES {
            return Err(Rejected::NotesTooLarge {
                bytes: self.notes.len(),
            });
        }
        let mut errors = Vec::new();
        check_project("project", &self.project, known, &mut errors);
        check_version("version", &self.version, &mut errors);
        if let Some(environment) = &self.environment {
            check_environment("environment", environment, &mut errors);
        }
        if let Some(title) = &self.title {
            check_title("title", title, &mut errors);
        }
        if let Some(url) = &self.url {
            check_url("url", url, &mut errors);
        }
        check_instant("released_at", self.released_at, &mut errors);
        check_instant("deployed_at", self.deployed_at, &mut errors);
        into_result(errors).map_err(Rejected::Invalid)
    }
}

/// Why [`NewRelease::validate`] refused a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// The notes are over [`MAX_NOTES_BYTES`]: the request is too large,
    /// not merely invalid.
    NotesTooLarge {
        /// Their size.
        bytes: usize,
    },
    /// Fields break rules, each at its path.
    Invalid(Vec<FieldError>),
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejected::NotesTooLarge { bytes } => write!(
                f,
                "notes must be at most {MAX_NOTES_BYTES} bytes; these are {bytes}"
            ),
            Rejected::Invalid(errors) => {
                let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
                f.write_str(&errors.join("; "))
            }
        }
    }
}

/// `POST /api/v1/releases/{project}/{version}/deployed`: record that a
/// version was deployed. The body may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeployStamp {
    /// Defaults to `production`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 32, pattern = "^[A-Za-z0-9][A-Za-z0-9._+-]{0,31}$")]
    pub environment: Option<String>,
    /// Defaults to now; at most an hour past the server's clock. The
    /// release keeps the latest deploy time it is given, so a stamp
    /// earlier than the recorded one changes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployed_at: Option<Timestamp>,
}

impl DeployStamp {
    /// The environment, with the default filled in.
    #[must_use]
    pub fn environment(&self) -> &str {
        self.environment.as_deref().unwrap_or(DEFAULT_ENVIRONMENT)
    }

    /// Checks the stamp and the project and version it is for.
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path; the path parameters
    /// are reported as `project` and `version`.
    pub fn validate(
        &self,
        project: &str,
        version: &str,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        check_project("project", project, known, &mut errors);
        check_version("version", version, &mut errors);
        if let Some(environment) = &self.environment {
            check_environment("environment", environment, &mut errors);
        }
        check_instant("deployed_at", self.deployed_at, &mut errors);
        into_result(errors)
    }
}

/// `GET /api/v1/releases`: published releases, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReleaseList {
    /// Newest `released_at` first, ties by newest id.
    pub releases: Vec<Release>,
    /// Whether more matched than `limit`.
    pub truncated: bool,
    /// Pass as `before` to get the next, older page; `null` on the last.
    pub next_before: Option<String>,
}

/// A place in the release list, after which the next page starts: the
/// last release's `released_at` and id. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseCursor {
    /// The last release's `released_at`.
    pub released_at: Timestamp,
    /// The last release's id.
    pub id: i64,
}

impl ReleaseCursor {
    /// The cursor just past `release`, if it is published.
    #[must_use]
    pub fn after(release: &Release) -> Option<Self> {
        release.released_at.map(|released_at| ReleaseCursor {
            released_at,
            id: release.id,
        })
    }
}

impl fmt::Display for ReleaseCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.released_at.as_millis(), self.id)
    }
}

impl FromStr for ReleaseCursor {
    type Err = BadCursor;

    fn from_str(text: &str) -> Result<Self, BadCursor> {
        let (at, id) = text.split_once('_').ok_or(BadCursor)?;
        Ok(ReleaseCursor {
            released_at: Timestamp::from_millis(at.parse().map_err(|_| BadCursor)?),
            id: id.parse().map_err(|_| BadCursor)?,
        })
    }
}

/// A cursor that no response handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadCursor;

impl fmt::Display for BadCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("is not a cursor from a previous page")
    }
}

fn check_project(
    path: &str,
    project: &str,
    known: &dyn Fn(&str) -> bool,
    errors: &mut Vec<FieldError>,
) {
    if !known(project) {
        errors.push(FieldError::new(
            path,
            format!("no [[project]] has id {project:?}"),
        ));
    }
}

/// Checks a version: it appears in URLs, so it is a short run of
/// URL-safe ASCII that cannot be `.`, `..` or hold a `/`.
pub fn check_version(path: &str, version: &str, errors: &mut Vec<FieldError>) {
    check_name(path, version, MAX_VERSION_LENGTH, errors);
}

/// Checks an environment name, by the same rule as a version.
pub fn check_environment(path: &str, environment: &str, errors: &mut Vec<FieldError>) {
    check_name(path, environment, MAX_ENVIRONMENT_LENGTH, errors);
}

/// 1 to `max` characters from `[A-Za-z0-9._+-]`, the first a letter or
/// digit.
fn check_name(path: &str, name: &str, max: usize, errors: &mut Vec<FieldError>) {
    let rule = format!(
        "must be 1 to {max} ASCII letters, digits, '.', '_', '+' or '-', starting with a letter or digit"
    );
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if !starts_well || !rest_ok || name.len() > max {
        errors.push(FieldError::new(path, format!("{rule}; got {name:?}")));
    }
}

/// A link is shown as one, so it follows the configuration's rule for
/// links, [`http_url`], and is at most [`MAX_URL_LENGTH`] bytes.
fn check_url(path: &str, url: &str, errors: &mut Vec<FieldError>) {
    if url.len() > MAX_URL_LENGTH {
        errors.push(FieldError::new(
            path,
            format!("must be at most {MAX_URL_LENGTH} bytes"),
        ));
    } else if let Err(message) = http_url(url) {
        errors.push(FieldError::new(path, message));
    }
}

fn check_instant(path: &str, instant: Option<Timestamp>, errors: &mut Vec<FieldError>) {
    if instant.is_some_and(|instant| instant.as_millis() < 0) {
        errors.push(FieldError::new(path, "must not be before 1970"));
    }
}

/// One entry on the timeline. Every kind carries `at`, the instant it is
/// placed at, and the `id` of its row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEvent {
    /// A check failed: at the outage's start.
    Outage {
        /// When it began.
        at: Timestamp,
        /// The outage's id.
        id: i64,
        /// The node that saw it.
        node_id: String,
        /// The failing check.
        check_id: String,
        /// The check's component when the outage opened, if any.
        component_id: Option<String>,
        /// `degraded` or `down`.
        health: Health,
        /// When it ended; `null` while it is open.
        ended_at: Option<Timestamp>,
    },
    /// An incident or maintenance was opened: at its creation.
    Incident {
        /// When it was opened.
        at: Timestamp,
        /// The incident's id.
        id: i64,
        /// Incident or maintenance.
        incident_kind: IncidentKind,
        /// Its title.
        title: String,
        /// Its impact.
        impact: Impact,
        /// Its current status.
        status: IncidentStatus,
        /// The components it affects.
        components: Vec<String>,
    },
    /// A release was deployed: at `deployed_at`.
    Deploy {
        /// When it was deployed.
        at: Timestamp,
        /// The release's id.
        id: i64,
        /// The release.
        release: Release,
    },
    /// A release was published and not yet deployed: at `released_at`.
    Release {
        /// When it was published.
        at: Timestamp,
        /// The release's id.
        id: i64,
        /// The release.
        release: Release,
    },
}

impl TimelineEvent {
    /// The instant the event is placed at.
    #[must_use]
    pub fn at(&self) -> Timestamp {
        match self {
            TimelineEvent::Outage { at, .. }
            | TimelineEvent::Incident { at, .. }
            | TimelineEvent::Deploy { at, .. }
            | TimelineEvent::Release { at, .. } => *at,
        }
    }

    /// The order kinds take at the same instant: outage, incident, deploy,
    /// release.
    #[must_use]
    pub fn kind_rank(&self) -> u8 {
        match self {
            TimelineEvent::Outage { .. } => 0,
            TimelineEvent::Incident { .. } => 1,
            TimelineEvent::Deploy { .. } => 2,
            TimelineEvent::Release { .. } => 3,
        }
    }

    /// The id of the event's row.
    #[must_use]
    pub fn id(&self) -> i64 {
        match self {
            TimelineEvent::Outage { id, .. }
            | TimelineEvent::Incident { id, .. }
            | TimelineEvent::Deploy { id, .. }
            | TimelineEvent::Release { id, .. } => *id,
        }
    }

    /// The timeline's total order: by time, then kind, then id.
    #[must_use]
    pub fn sort_key(&self) -> (Timestamp, u8, i64) {
        (self.at(), self.kind_rank(), self.id())
    }
}

/// `GET /api/v1/timeline`: outages, incidents and deploys, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Timeline {
    /// The start of the window: events at or after it.
    pub since: Timestamp,
    /// Oldest first; ties by kind (outage, incident, deploy, release),
    /// then id.
    pub events: Vec<TimelineEvent>,
    /// Whether more events fell in the window than `limit`.
    pub truncated: bool,
    /// Pass as `after`, with the same `since`, to get the next page;
    /// `null` on the last.
    pub next_after: Option<String>,
}

/// A place on the timeline: an event's [`TimelineEvent::sort_key`].
/// Paging resumes strictly past it, so events sharing an instant are
/// neither repeated nor skipped. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineCursor {
    /// The event's instant.
    pub at: Timestamp,
    /// Its [`TimelineEvent::kind_rank`].
    pub kind_rank: u8,
    /// Its row's id.
    pub id: i64,
}

impl TimelineCursor {
    /// The cursor at `event`.
    #[must_use]
    pub fn at(event: &TimelineEvent) -> Self {
        let (at, kind_rank, id) = event.sort_key();
        TimelineCursor { at, kind_rank, id }
    }
}

impl fmt::Display for TimelineCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}_{}", self.at.as_millis(), self.kind_rank, self.id)
    }
}

impl FromStr for TimelineCursor {
    type Err = BadCursor;

    fn from_str(text: &str) -> Result<Self, BadCursor> {
        let mut parts = text.split('_');
        let (Some(at), Some(kind_rank), Some(id), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(BadCursor);
        };
        let kind_rank: u8 = kind_rank.parse().map_err(|_| BadCursor)?;
        if kind_rank > 3 {
            return Err(BadCursor);
        }
        Ok(TimelineCursor {
            at: Timestamp::from_millis(at.parse().map_err(|_| BadCursor)?),
            kind_rank,
            id: id.parse().map_err(|_| BadCursor)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_errors(version: &str) -> Vec<FieldError> {
        let mut errors = Vec::new();
        check_version("version", version, &mut errors);
        errors
    }

    #[test]
    fn versions_are_url_safe_ascii() {
        for good in [
            "1",
            "v1.4.0",
            "2026.10.23+5",
            "1.0.0-rc.1",
            "release_7",
            "a..b",
        ] {
            assert!(version_errors(good).is_empty(), "{good}");
        }
        let long = "a".repeat(MAX_VERSION_LENGTH + 1);
        for bad in [
            "", ".", "..", "../etc", "1/2", "v1/../x", "-1", "+1", "_1", "v1 2", "v1?x", "v1#x",
            "v1%2F", "vé", "v1\u{0}", "١٢", &long,
        ] {
            assert_eq!(version_errors(bad).len(), 1, "{bad:?}");
        }
        assert_eq!(version_errors(&"a".repeat(MAX_VERSION_LENGTH)), []);
    }

    fn release(title: Option<&str>, notes: &str, url: Option<&str>) -> NewRelease {
        NewRelease {
            project: "api".to_owned(),
            version: "v1".to_owned(),
            environment: None,
            title: title.map(ToOwned::to_owned),
            notes: notes.to_owned(),
            url: url.map(ToOwned::to_owned),
            released_at: None,
            deployed_at: None,
        }
    }

    fn paths(new: &NewRelease) -> Vec<String> {
        match new.validate(&|id| id == "api") {
            Ok(()) => Vec::new(),
            Err(Rejected::Invalid(errors)) => errors.into_iter().map(|error| error.path).collect(),
            Err(too_large) => panic!("{too_large}"),
        }
    }

    #[test]
    fn defaults_and_limits() {
        let new = release(None, "", None);
        assert_eq!(new.environment(), "production");
        assert_eq!(new.title(), "v1");
        assert_eq!(paths(&new), Vec::<String>::new());
        assert_eq!(paths(&release(Some(&"t".repeat(201)), "", None)), ["title"]);
        assert_eq!(
            paths(&release(Some(&"t".repeat(200)), "", None)),
            Vec::<String>::new()
        );
        // Too large is its own answer, ahead of every other problem.
        let mut huge = release(Some(""), &"n".repeat(MAX_NOTES_BYTES + 1), Some("x"));
        huge.project = "nope".to_owned();
        assert_eq!(
            huge.validate(&|id| id == "api"),
            Err(Rejected::NotesTooLarge {
                bytes: MAX_NOTES_BYTES + 1
            })
        );
        assert_eq!(
            paths(&release(None, &"n".repeat(MAX_NOTES_BYTES), None)),
            Vec::<String>::new()
        );
        let mut unknown = release(None, "", None);
        unknown.project = "nope".to_owned();
        assert_eq!(paths(&unknown), ["project"]);
    }

    #[test]
    fn links_are_http_only() {
        for good in ["https://example.com/v1", "http://example.com"] {
            assert!(paths(&release(None, "", Some(good))).is_empty(), "{good}");
        }
        for bad in [
            "javascript:alert(1)",
            "JavaScript://x%0aalert(1)",
            "data:text/html,x",
            "//example.com",
            "https://",
            "https:///path",
            "https://example.com/a b",
            "ftp://example.com",
            &format!("https://example.com/{}", "a".repeat(MAX_URL_LENGTH)),
        ] {
            assert_eq!(paths(&release(None, "", Some(bad))), ["url"], "{bad}");
        }
    }

    #[test]
    fn cursors_round_trip_and_refuse_anything_else() {
        let timeline = TimelineCursor {
            at: Timestamp::from_millis(1_700_000_000_000),
            kind_rank: 2,
            id: 41,
        };
        assert_eq!(timeline.to_string().parse(), Ok(timeline));
        let release = ReleaseCursor {
            released_at: Timestamp::from_millis(1_700_000_000_000),
            id: 7,
        };
        assert_eq!(release.to_string().parse(), Ok(release));
        for bad in ["", "1", "1_2", "1_2_3_4", "x_2_3", "1_4_3", "1_2_y"] {
            assert_eq!(bad.parse::<TimelineCursor>(), Err(BadCursor), "{bad}");
        }
        for bad in ["", "1", "1_x", "x_1", "1_2_3"] {
            assert_eq!(bad.parse::<ReleaseCursor>(), Err(BadCursor), "{bad}");
        }
    }
}
