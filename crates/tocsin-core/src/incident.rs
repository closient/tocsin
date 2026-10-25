// SPDX-License-Identifier: AGPL-3.0-only

//! Incidents and scheduled maintenance: the model, the write requests and
//! the rules every write must follow.
//!
//! Incidents are data, not configuration. An incident is opened with a
//! first update, gains further updates (append-only) and can have its
//! title, impact, components and schedule edited. Scheduled maintenance is
//! an incident of kind [`IncidentKind::Maintenance`] with a schedule.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use utoipa::ToSchema;
use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat, Type};
use utoipa::openapi::{RefOr, Schema};

/// Defines a fieldless enum stored and sent as fixed lowercase text, with
/// `ALL`, `as_str`, `Display` and `FromStr`.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $unknown:literal {
            $($(#[$variant_meta:meta])* $variant:ident => $text:literal,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema, ::schemars::JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            /// Every value, in order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The stable name used in storage, JSON and the CLI.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = String;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                $name::ALL
                    .iter()
                    .copied()
                    .find(|value| value.as_str() == text)
                    .ok_or_else(|| {
                        let names: Vec<&str> = $name::ALL.iter().map(|value| value.as_str()).collect();
                        format!("unknown {} {text:?}; expected one of {}", $unknown, names.join(", "))
                    })
            }
        }
    };
}

pub(crate) use text_enum;

text_enum! {
    /// Whether something broke or work was planned.
    IncidentKind, "kind" {
        /// Something went wrong.
        Incident => "incident",
        /// Planned work, announced ahead of time with a schedule.
        Maintenance => "maintenance",
    }
}

text_enum! {
    /// How badly the affected components are hit. Ordered least to worst.
    Impact, "impact" {
        /// No visible effect.
        None => "none",
        /// Some users notice.
        Minor => "minor",
        /// Most users notice.
        Major => "major",
        /// The service is unusable.
        Critical => "critical",
    }
}

text_enum! {
    /// Where an incident or maintenance stands. The first four belong to
    /// incidents, the last three to maintenance; see
    /// [`IncidentStatus::allowed_for`].
    IncidentStatus, "status" {
        /// The cause is not known yet.
        Investigating => "investigating",
        /// The cause is known; a fix is under way.
        Identified => "identified",
        /// A fix is out; watching that it holds.
        Monitoring => "monitoring",
        /// Over.
        Resolved => "resolved",
        /// Maintenance announced, not started.
        Scheduled => "scheduled",
        /// Maintenance under way.
        InProgress => "in_progress",
        /// Maintenance over.
        Completed => "completed",
    }
}

impl IncidentStatus {
    /// Whether an incident of `kind` can have this status.
    #[must_use]
    pub const fn allowed_for(self, kind: IncidentKind) -> bool {
        match kind {
            IncidentKind::Incident => matches!(
                self,
                IncidentStatus::Investigating
                    | IncidentStatus::Identified
                    | IncidentStatus::Monitoring
                    | IncidentStatus::Resolved
            ),
            IncidentKind::Maintenance => matches!(
                self,
                IncidentStatus::Scheduled | IncidentStatus::InProgress | IncidentStatus::Completed
            ),
        }
    }

    /// The status a new incident of `kind` starts with when none is given.
    #[must_use]
    pub const fn initial(kind: IncidentKind) -> Self {
        match kind {
            IncidentKind::Incident => IncidentStatus::Investigating,
            IncidentKind::Maintenance => IncidentStatus::Scheduled,
        }
    }

    /// Whether the incident is over: `resolved` or `completed`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, IncidentStatus::Resolved | IncidentStatus::Completed)
    }

    /// The statuses an incident of `kind` can have, for messages.
    #[must_use]
    pub fn names_for(kind: IncidentKind) -> String {
        let names: Vec<&str> = IncidentStatus::ALL
            .iter()
            .filter(|status| status.allowed_for(kind))
            .map(|status| status.as_str())
            .collect();
        names.join(", ")
    }
}

text_enum! {
    /// How a writer was identified.
    ActorKind, "actor kind" {
        /// A `[[token]]` from the configuration, by its id.
        Token => "token",
        /// A person, as named by the authenticating proxy in front of tocsin.
        Person => "person",
    }
}

/// Who wrote a change: recorded with every incident, update and edit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Actor {
    /// How the writer was identified.
    pub kind: ActorKind,
    /// The token's id, or the person's identity from the proxy.
    pub id: String,
}

impl Actor {
    /// A writer identified by a configured token.
    #[must_use]
    pub fn token(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorKind::Token,
            id: id.into(),
        }
    }

    /// A person, identified by the email address an identity-aware proxy
    /// vouched for.
    #[must_use]
    pub fn person(email: impl Into<String>) -> Self {
        Actor {
            kind: ActorKind::Person,
            id: email.into(),
        }
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

/// An instant, held as unix milliseconds UTC and written as RFC 3339 with
/// millisecond precision in UTC (`2026-10-23T14:05:00.000Z`). Any RFC 3339
/// offset is accepted on input; precision below a millisecond is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The instant `ms` milliseconds after the unix epoch.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Timestamp(ms)
    }

    /// Milliseconds since the unix epoch.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match jiff::Timestamp::from_millisecond(self.0) {
            Ok(instant) => write!(f, "{instant:.3}"),
            // Outside jiff's range (years -9999 to 9999): still say something.
            Err(_) => write!(f, "{}ms", self.0),
        }
    }
}

impl FromStr for Timestamp {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let instant: jiff::Timestamp = text.parse().map_err(|error| {
            format!("{text:?} is not an RFC 3339 timestamp such as 2026-10-23T14:05:00Z: {error}")
        })?;
        Ok(Timestamp(instant.as_millisecond()))
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Timestamp {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::String)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime)))
            .description(Some(
                "RFC 3339; written in UTC with millisecond precision, read with any offset.",
            ))
            .examples(["2026-10-23T14:05:00.000Z"])
            .into()
    }
}

impl ToSchema for Timestamp {}

impl JsonSchema for Timestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Timestamp".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time",
            "description": "RFC 3339; written in UTC with millisecond precision, read with any offset.",
            "examples": ["2026-10-23T14:05:00.000Z"],
        })
    }
}

/// An incident or scheduled maintenance, with every update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Incident {
    /// Assigned by tocsin; never reused.
    pub id: i64,
    /// Incident or maintenance. Fixed at creation.
    pub kind: IncidentKind,
    /// One line, shown on the status page.
    pub title: String,
    /// How badly the affected components are hit.
    pub impact: Impact,
    /// The status of the latest update.
    pub status: IncidentStatus,
    /// Ids of the affected `[[component]]`s, sorted.
    pub components: Vec<String>,
    /// When maintenance is planned to begin; always set for maintenance,
    /// never for an incident.
    pub scheduled_start: Option<Timestamp>,
    /// When maintenance is planned to end; after `scheduled_start`.
    pub scheduled_end: Option<Timestamp>,
    /// When the latest update moved it to `resolved` or `completed`; cleared
    /// if a later update reopens it.
    pub resolved_at: Option<Timestamp>,
    /// When it was opened.
    pub created_at: Timestamp,
    /// Who opened it.
    pub created_by: Actor,
    /// When it last changed: an update or an edit.
    pub updated_at: Timestamp,
    /// Who made the last change.
    pub updated_by: Actor,
    /// When it was published: shown publicly and announced to subscribers.
    /// `null` while it is a draft, which only trusted readers see.
    #[serde(default)]
    pub published_at: Option<Timestamp>,
    /// Who published it; `null` while it is a draft.
    #[serde(default)]
    pub published_by: Option<Actor>,
    /// Every update, oldest first. The first is the opening message.
    pub updates: Vec<IncidentUpdate>,
}

impl Incident {
    /// Whether it is still a draft: not yet published.
    #[must_use]
    pub fn is_draft(&self) -> bool {
        self.published_at.is_none()
    }
}

/// One entry in an incident's timeline. Updates are never changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IncidentUpdate {
    /// Assigned by tocsin.
    pub id: i64,
    /// The status the incident moved to (or stayed at).
    pub status: IncidentStatus,
    /// Markdown.
    pub message: String,
    /// When it was posted.
    pub created_at: Timestamp,
    /// Who posted it.
    pub created_by: Actor,
}

/// `POST /api/v1/incidents`: open an incident or announce maintenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewIncident {
    /// Defaults to `incident`.
    #[serde(default = "default_kind")]
    pub kind: IncidentKind,
    /// One line, not empty, at most 200 characters.
    #[schema(max_length = 200)]
    #[schemars(length(max = 200))]
    pub title: String,
    /// Defaults to `none`.
    #[serde(default = "default_impact")]
    pub impact: Impact,
    /// Ids of configured components. May be empty.
    #[serde(default)]
    pub components: Vec<String>,
    /// The first update's status. Defaults to `investigating` for an
    /// incident and `scheduled` for maintenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IncidentStatus>,
    /// The first update, in markdown. Not empty.
    pub message: String,
    /// Required for maintenance, refused for an incident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_start: Option<Timestamp>,
    /// Required for maintenance, after `scheduled_start`; refused for an
    /// incident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_end: Option<Timestamp>,
    /// Whether subscribers hear about it. `false` opens it quietly. It
    /// changes nothing else.
    #[serde(
        default = "notify_by_default",
        skip_serializing_if = "is_notify_default"
    )]
    #[schema(default = true)]
    #[schemars(default = "notify_by_default")]
    pub notify: bool,
    /// `true` saves it as a draft: internal only, announced to nobody, until
    /// a person publishes it. Left out, it is published when the caller may
    /// publish (the `incident` scope, or a person) and a draft otherwise
    /// (the `incident:draft` scope); the MCP tool always defaults to a
    /// draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

fn default_kind() -> IncidentKind {
    IncidentKind::Incident
}

fn default_impact() -> Impact {
    Impact::None
}

/// `POST /api/v1/incidents/{id}/updates`: append an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewUpdate {
    /// The status the incident moves to; it must suit the incident's kind.
    pub status: IncidentStatus,
    /// Markdown. Not empty.
    pub message: String,
    /// Whether email subscribers hear about it. `false` posts it quietly,
    /// for a typo fix or a minor note. It changes nothing else.
    #[serde(
        default = "notify_by_default",
        skip_serializing_if = "is_notify_default"
    )]
    #[schema(default = true)]
    pub notify: bool,
}

fn notify_by_default() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference
fn is_notify_default(notify: &bool) -> bool {
    *notify
}

/// `PATCH /api/v1/incidents/{id}`: change what an incident says. Only the
/// fields given change; status changes only through an update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentEdit {
    /// A new title, not empty, at most 200 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 200)]
    pub title: Option<String>,
    /// A new impact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<Impact>,
    /// The full new set of affected components (not a delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<String>>,
    /// A new planned start. Maintenance only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_start: Option<Timestamp>,
    /// A new planned end. Maintenance only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_end: Option<Timestamp>,
}

impl IncidentEdit {
    /// Whether the edit changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &IncidentEdit::default()
    }
}

/// `GET /api/v1/incidents`: one page of incidents, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IncidentList {
    /// Newest first.
    pub incidents: Vec<Incident>,
    /// Pass as `before` to get the next page; `null` on the last page.
    pub next_before: Option<i64>,
}

/// One problem with a request, at a field path such as `components[1]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FieldError {
    /// Where in the request body; empty for the body as a whole.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl FieldError {
    /// A problem at `path`.
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        FieldError {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Checks a component list against the configured ids.
fn check_components(
    path: &str,
    components: &[String],
    known: &dyn Fn(&str) -> bool,
    errors: &mut Vec<FieldError>,
) {
    for (index, id) in components.iter().enumerate() {
        if !known(id) {
            errors.push(FieldError::new(
                format!("{path}[{index}]"),
                format!("no [[component]] has id {id:?}"),
            ));
        } else if components[..index].contains(id) {
            errors.push(FieldError::new(
                format!("{path}[{index}]"),
                format!("component {id:?} is listed twice"),
            ));
        }
    }
}

pub(crate) fn check_text(path: &str, text: &str, errors: &mut Vec<FieldError>) {
    if text.trim().is_empty() {
        errors.push(FieldError::new(path, "must not be empty"));
    }
}

/// The longest title, in characters. The `max_length` on each title field's
/// schema repeats it for the OpenAPI document.
pub const MAX_TITLE_LENGTH: usize = 200;

/// A title is one line: besides being non-empty, it holds no control
/// characters (CR, LF, tab, ...) and no Unicode line or paragraph
/// separator, any of which would break the line it is printed on. It is at
/// most [`MAX_TITLE_LENGTH`] characters.
pub(crate) fn check_title(path: &str, title: &str, errors: &mut Vec<FieldError>) {
    check_text(path, title, errors);
    let length = title.chars().count();
    if length > MAX_TITLE_LENGTH {
        errors.push(FieldError::new(
            path,
            format!("must be at most {MAX_TITLE_LENGTH} characters; this is {length}"),
        ));
    }
    if let Some(found) = title
        .chars()
        .find(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        errors.push(FieldError::new(
            path,
            format!(
                "must be a single line without control characters; found U+{:04X}",
                u32::from(found)
            ),
        ));
    }
}

/// What to do about a schedule on a new incident.
const SCHEDULE_ON_CREATE: &str = "only maintenance has a schedule; set kind to \"maintenance\"";

/// What to do about a schedule added to an existing incident, whose kind
/// cannot be changed.
const SCHEDULE_ON_EDIT: &str = "only maintenance has a schedule, and an incident's kind is fixed \
     at creation; announce the window as a new incident with kind \"maintenance\"";

/// Checks the schedule `kind` requires: both ends for maintenance, the end
/// after the start; neither for an incident, which is refused with
/// `advice`.
fn check_schedule(
    kind: IncidentKind,
    start: Option<Timestamp>,
    end: Option<Timestamp>,
    advice: &str,
    errors: &mut Vec<FieldError>,
) {
    match kind {
        IncidentKind::Maintenance => match (start, end) {
            (Some(start), Some(end)) if end <= start => errors.push(FieldError::new(
                "scheduled_end",
                "must be after scheduled_start",
            )),
            (Some(_), Some(_)) => {}
            (start, end) => {
                if start.is_none() {
                    errors.push(FieldError::new(
                        "scheduled_start",
                        "maintenance needs a scheduled start",
                    ));
                }
                if end.is_none() {
                    errors.push(FieldError::new(
                        "scheduled_end",
                        "maintenance needs a scheduled end",
                    ));
                }
            }
        },
        IncidentKind::Incident => {
            for (path, value) in [("scheduled_start", start), ("scheduled_end", end)] {
                if value.is_some() {
                    errors.push(FieldError::new(path, advice));
                }
            }
        }
    }
}

fn check_status(kind: IncidentKind, status: IncidentStatus, errors: &mut Vec<FieldError>) {
    if !status.allowed_for(kind) {
        errors.push(FieldError::new(
            "status",
            format!(
                "{kind} cannot be {status}; expected one of {}",
                IncidentStatus::names_for(kind)
            ),
        ));
    }
}

pub(crate) fn into_result(errors: Vec<FieldError>) -> Result<(), Vec<FieldError>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

impl NewIncident {
    /// The status the first update gets.
    #[must_use]
    pub fn first_status(&self) -> IncidentStatus {
        self.status
            .unwrap_or_else(|| IncidentStatus::initial(self.kind))
    }

    /// Checks every rule a new incident must follow. `known` says whether a
    /// component id is configured.
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path.
    pub fn validate(&self, known: &dyn Fn(&str) -> bool) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        check_title("title", &self.title, &mut errors);
        check_text("message", &self.message, &mut errors);
        check_components("components", &self.components, known, &mut errors);
        check_status(self.kind, self.first_status(), &mut errors);
        check_schedule(
            self.kind,
            self.scheduled_start,
            self.scheduled_end,
            SCHEDULE_ON_CREATE,
            &mut errors,
        );
        into_result(errors)
    }
}

impl NewUpdate {
    /// Checks the update's own fields: a non-empty message.
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path.
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        check_text("message", &self.message, &mut errors);
        into_result(errors)
    }
}

impl IncidentEdit {
    /// Checks the edit's own fields. `known` says whether a component id is
    /// configured. Rules that depend on the incident are checked by
    /// [`Incident::apply_edit`].
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path.
    pub fn validate(&self, known: &dyn Fn(&str) -> bool) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        if self.is_empty() {
            errors.push(FieldError::new(
                "",
                "nothing to change: give at least one of title, impact, components, \
                 scheduled_start, scheduled_end",
            ));
        }
        if let Some(title) = &self.title {
            check_title("title", title, &mut errors);
        }
        if let Some(components) = &self.components {
            check_components("components", components, known, &mut errors);
        }
        into_result(errors)
    }
}

impl Incident {
    /// Checks that `update` can be appended: its status must suit this
    /// incident's kind.
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path.
    pub fn check_update(&self, update: &NewUpdate) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        check_status(self.kind, update.status, &mut errors);
        into_result(errors)
    }

    /// Applies `edit` to this incident's fields, leaving the audit fields
    /// alone. Components are stored sorted.
    ///
    /// # Errors
    ///
    /// The result breaks a rule: a schedule on an incident, or a
    /// maintenance window that ends before it starts.
    pub fn apply_edit(&mut self, edit: &IncidentEdit) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        if self.kind == IncidentKind::Incident {
            check_schedule(
                self.kind,
                edit.scheduled_start,
                edit.scheduled_end,
                SCHEDULE_ON_EDIT,
                &mut errors,
            );
        } else {
            check_schedule(
                self.kind,
                edit.scheduled_start.or(self.scheduled_start),
                edit.scheduled_end.or(self.scheduled_end),
                SCHEDULE_ON_EDIT,
                &mut errors,
            );
        }
        into_result(errors)?;
        if let Some(title) = &edit.title {
            title.clone_into(&mut self.title);
        }
        if let Some(impact) = edit.impact {
            self.impact = impact;
        }
        if let Some(components) = &edit.components {
            let mut components = components.clone();
            components.sort();
            self.components = components;
        }
        if edit.scheduled_start.is_some() {
            self.scheduled_start = edit.scheduled_start;
        }
        if edit.scheduled_end.is_some() {
            self.scheduled_end = edit.scheduled_end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(id: &str) -> bool {
        matches!(id, "api" | "web")
    }

    fn new_incident() -> NewIncident {
        NewIncident {
            kind: IncidentKind::Incident,
            title: "API errors".into(),
            impact: Impact::Major,
            components: vec!["api".into()],
            status: None,
            message: "Looking into it.".into(),
            scheduled_start: None,
            scheduled_end: None,
            notify: true,
            draft: None,
        }
    }

    fn paths(errors: Vec<FieldError>) -> Vec<String> {
        errors.into_iter().map(|error| error.path).collect()
    }

    #[test]
    fn enums_round_trip_through_text_and_json() {
        for status in IncidentStatus::ALL {
            assert_eq!(status.as_str().parse::<IncidentStatus>(), Ok(*status));
            let json = serde_json::to_value(status).unwrap();
            assert_eq!(json, status.as_str());
        }
        assert_eq!(
            serde_json::to_value(IncidentStatus::InProgress).unwrap(),
            "in_progress"
        );
        for impact in Impact::ALL {
            assert_eq!(impact.as_str().parse::<Impact>(), Ok(*impact));
        }
        let error = "sideways".parse::<Impact>().unwrap_err();
        assert!(error.contains("none, minor, major, critical"), "{error}");
    }

    #[test]
    fn each_kind_has_its_own_statuses() {
        let incident: Vec<_> = IncidentStatus::ALL
            .iter()
            .filter(|status| status.allowed_for(IncidentKind::Incident))
            .collect();
        assert_eq!(incident.len(), 4);
        assert!(IncidentStatus::Completed.allowed_for(IncidentKind::Maintenance));
        assert!(!IncidentStatus::Resolved.allowed_for(IncidentKind::Maintenance));
        assert!(
            IncidentStatus::initial(IncidentKind::Maintenance)
                .allowed_for(IncidentKind::Maintenance)
        );
    }

    #[test]
    fn timestamps_are_utc_milliseconds_and_read_any_offset() {
        let instant: Timestamp = "2026-10-23T16:05:00.123456+02:00".parse().unwrap();
        assert_eq!(instant.to_string(), "2026-10-23T14:05:00.123Z");
        assert_eq!(
            serde_json::to_value(instant).unwrap(),
            "2026-10-23T14:05:00.123Z"
        );
        assert_eq!(
            Timestamp::from_millis(0).to_string(),
            "1970-01-01T00:00:00.000Z"
        );
        assert!("2026-10-23 14:05".parse::<Timestamp>().is_err());
        assert!(serde_json::from_value::<Timestamp>("yesterday".into()).is_err());
    }

    #[test]
    fn a_valid_incident_passes() {
        assert_eq!(new_incident().validate(&known), Ok(()));
    }

    #[test]
    fn every_problem_is_reported_at_its_path() {
        let incident = NewIncident {
            title: "  ".into(),
            message: String::new(),
            components: vec!["api".into(), "db".into(), "api".into()],
            status: Some(IncidentStatus::Completed),
            scheduled_start: Some(Timestamp::from_millis(0)),
            ..new_incident()
        };
        assert_eq!(
            paths(incident.validate(&known).unwrap_err()),
            [
                "title",
                "message",
                "components[1]",
                "components[2]",
                "status",
                "scheduled_start"
            ]
        );
    }

    #[test]
    fn a_title_is_one_line_on_create_and_on_edit() {
        for title in [
            "API\nerrors",
            "API errors\r",
            "API\terrors",
            "API\u{85}errors",
            "API\u{2028}errors",
        ] {
            let incident = NewIncident {
                title: title.into(),
                ..new_incident()
            };
            let errors = incident.validate(&known).unwrap_err();
            assert_eq!(paths(errors.clone()), ["title"], "{title:?}");
            assert!(errors[0].message.contains("single line"), "{errors:?}");
            let edit = IncidentEdit {
                title: Some(title.into()),
                ..IncidentEdit::default()
            };
            assert_eq!(
                paths(edit.validate(&known).unwrap_err()),
                ["title"],
                "{title:?}"
            );
        }
        let unicode = NewIncident {
            title: "Paiement refusé — API ✓".into(),
            ..new_incident()
        };
        assert_eq!(unicode.validate(&known), Ok(()));
    }

    #[test]
    fn a_title_is_at_most_200_characters_on_create_and_on_edit() {
        // Characters, not bytes: 200 two-byte characters fit.
        let longest = NewIncident {
            title: "é".repeat(MAX_TITLE_LENGTH),
            ..new_incident()
        };
        assert_eq!(longest.validate(&known), Ok(()));
        let long = "x".repeat(MAX_TITLE_LENGTH + 1);
        let incident = NewIncident {
            title: long.clone(),
            ..new_incident()
        };
        let errors = incident.validate(&known).unwrap_err();
        assert_eq!(paths(errors.clone()), ["title"]);
        assert!(errors[0].message.contains("at most 200"), "{errors:?}");
        let edit = IncidentEdit {
            title: Some(long),
            ..IncidentEdit::default()
        };
        assert_eq!(paths(edit.validate(&known).unwrap_err()), ["title"]);
    }

    #[test]
    fn a_new_incident_with_a_schedule_is_told_to_be_maintenance() {
        let incident = NewIncident {
            scheduled_end: Some(Timestamp::from_millis(9000)),
            ..new_incident()
        };
        let errors = incident.validate(&known).unwrap_err();
        assert!(errors[0].message.contains("set kind"), "{errors:?}");
    }

    #[test]
    fn maintenance_needs_a_forward_schedule() {
        let maintenance = NewIncident {
            kind: IncidentKind::Maintenance,
            ..new_incident()
        };
        assert_eq!(
            paths(maintenance.validate(&known).unwrap_err()),
            ["scheduled_start", "scheduled_end"]
        );
        let backwards = NewIncident {
            scheduled_start: Some(Timestamp::from_millis(2000)),
            scheduled_end: Some(Timestamp::from_millis(1000)),
            ..maintenance.clone()
        };
        assert_eq!(
            paths(backwards.validate(&known).unwrap_err()),
            ["scheduled_end"]
        );
        let forward = NewIncident {
            scheduled_start: Some(Timestamp::from_millis(1000)),
            scheduled_end: Some(Timestamp::from_millis(2000)),
            ..maintenance
        };
        assert_eq!(forward.validate(&known), Ok(()));
        assert_eq!(forward.first_status(), IncidentStatus::Scheduled);
    }

    #[test]
    fn unknown_fields_are_refused() {
        let error = serde_json::from_value::<NewUpdate>(serde_json::json!({
            "status": "resolved", "message": "ok", "body": "typo"
        }))
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown field `body`"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_edit_is_refused() {
        assert_eq!(
            paths(IncidentEdit::default().validate(&known).unwrap_err()),
            [""]
        );
    }

    fn stored(kind: IncidentKind) -> Incident {
        let at = Timestamp::from_millis(1000);
        let actor = Actor::token("ci");
        Incident {
            id: 1,
            kind,
            title: "t".into(),
            impact: Impact::Minor,
            status: IncidentStatus::initial(kind),
            components: Vec::new(),
            scheduled_start: (kind == IncidentKind::Maintenance).then_some(at),
            scheduled_end: (kind == IncidentKind::Maintenance)
                .then_some(Timestamp::from_millis(5000)),
            resolved_at: None,
            created_at: at,
            created_by: actor.clone(),
            updated_at: at,
            updated_by: actor.clone(),
            published_at: Some(at),
            published_by: Some(actor),
            updates: Vec::new(),
        }
    }

    #[test]
    fn edits_keep_the_schedule_consistent() {
        let mut incident = stored(IncidentKind::Incident);
        let schedule = IncidentEdit {
            scheduled_end: Some(Timestamp::from_millis(9000)),
            ..IncidentEdit::default()
        };
        let errors = incident.apply_edit(&schedule).unwrap_err();
        assert_eq!(paths(errors.clone()), ["scheduled_end"]);
        assert!(
            errors[0].message.contains("fixed at creation")
                && !errors[0].message.contains("set kind"),
            "an edit cannot change kind, so must not suggest it: {}",
            errors[0].message
        );

        let mut maintenance = stored(IncidentKind::Maintenance);
        let before_start = IncidentEdit {
            scheduled_end: Some(Timestamp::from_millis(500)),
            ..IncidentEdit::default()
        };
        assert_eq!(
            paths(maintenance.apply_edit(&before_start).unwrap_err()),
            ["scheduled_end"]
        );
        let edit = IncidentEdit {
            title: Some("New".into()),
            components: Some(vec!["web".into(), "api".into()]),
            scheduled_end: Some(Timestamp::from_millis(9000)),
            ..IncidentEdit::default()
        };
        maintenance.apply_edit(&edit).unwrap();
        assert_eq!(maintenance.title, "New");
        assert_eq!(maintenance.components, ["api", "web"]);
        assert_eq!(
            maintenance.scheduled_end,
            Some(Timestamp::from_millis(9000))
        );
        assert_eq!(
            maintenance.scheduled_start,
            Some(Timestamp::from_millis(1000))
        );
    }

    #[test]
    fn updates_must_suit_the_kind() {
        let incident = stored(IncidentKind::Incident);
        let update = NewUpdate {
            status: IncidentStatus::Completed,
            message: "done".into(),
            notify: true,
        };
        assert_eq!(
            paths(incident.check_update(&update).unwrap_err()),
            ["status"]
        );
        let update = NewUpdate {
            status: IncidentStatus::Resolved,
            ..update
        };
        assert_eq!(incident.check_update(&update), Ok(()));
    }
}
