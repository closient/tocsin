// SPDX-License-Identifier: AGPL-3.0-only

//! Silences: holding back alert messages for a while, never the record.
//!
//! A silence is history, not configuration. It matches alerts by the
//! identity their rows carry (node, check or heartbeat, component,
//! notifier), always ends at a time set when it is made, and can be
//! scoped to one outage (an episode), ending early when that outage
//! closes. Nothing extends it: a longer silence is a new one. The
//! outage, the alert and every message the silence held back are still
//! recorded; only the sending is skipped.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::Health;
use crate::config::Duration;
use crate::incident::{Actor, FieldError, Timestamp, check_text, into_result, text_enum};
use crate::status::{CheckKind, OutageRecord};

/// Which alerts a silence holds back: every field set must equal the
/// alert's. A field left out matches anything.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Matchers {
    /// The node that saw the alert, as its `[server] node_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Whether `subject` names a check or a heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_kind: Option<CheckKind>,
    /// A `[[check]]` or `[[heartbeat]]` id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// A `[[component]]` id: the alert's subject's component when it last
    /// fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// A `[[notifier]]` id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifier: Option<String>,
}

/// An alert as a silence sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target<'a> {
    /// The node that saw it.
    pub node: &'a str,
    /// Check or heartbeat.
    pub subject_kind: CheckKind,
    /// The check or heartbeat id.
    pub subject: &'a str,
    /// The subject's component when it last fired.
    pub component: Option<&'a str>,
    /// The notifier it goes to.
    pub notifier: &'a str,
}

impl Matchers {
    /// Whether no field is set, which would match every alert.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node.is_none()
            && self.subject_kind.is_none()
            && self.subject.is_none()
            && self.component.is_none()
            && self.notifier.is_none()
    }

    /// Whether every field set equals `target`'s.
    #[must_use]
    pub fn matches(&self, target: &Target<'_>) -> bool {
        self.node.as_deref().is_none_or(|node| node == target.node)
            && self
                .subject_kind
                .is_none_or(|kind| kind == target.subject_kind)
            && self
                .subject
                .as_deref()
                .is_none_or(|subject| subject == target.subject)
            && self
                .component
                .as_deref()
                .is_none_or(|component| Some(component) == target.component)
            && self
                .notifier
                .as_deref()
                .is_none_or(|notifier| notifier == target.notifier)
    }
}

/// `POST /api/v1/silences`: hold back the alerts `matchers` names until
/// `expires_at`, or until outage `episode` closes if that is sooner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSilence {
    /// Which alerts it holds back. At least one field unless `episode` is
    /// set, which fills in the outage's node, subject kind and subject.
    #[serde(default)]
    pub matchers: Matchers,
    /// An outage id: the silence ends when that outage closes (the
    /// subject recovers, or moves between degraded and down), so a
    /// re-fire is sent. The outage must be open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<i64>,
    /// When it stops applying. Required, in the future, and at most
    /// `[server] max_silence` from now.
    pub expires_at: Timestamp,
    /// Why: shown wherever the silence is.
    pub reason: String,
}

/// The configured ids a silence's matchers may name.
pub struct Known<'a> {
    /// The kind of the check or heartbeat with this id, if one is
    /// configured.
    pub subject: &'a dyn Fn(&str) -> Option<CheckKind>,
    /// Whether a `[[component]]` has this id.
    pub component: &'a dyn Fn(&str) -> bool,
    /// Whether a `[[notifier]]` has this id.
    pub notifier: &'a dyn Fn(&str) -> bool,
}

impl NewSilence {
    /// Narrows the silence to outage `outage`, its `episode`: the
    /// outage's node, subject kind and subject become matchers.
    ///
    /// # Errors
    ///
    /// The outage has ended, or a matcher names a different node or
    /// subject than the outage's.
    pub fn scope_to(&mut self, outage: &OutageRecord) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        if outage.ended_at.is_some() {
            errors.push(FieldError::new(
                "episode",
                format!("outage {} has ended; silence the alert instead", outage.id),
            ));
        }
        let matchers = &mut self.matchers;
        fill(
            "matchers.node",
            &mut matchers.node,
            &outage.node_id,
            &mut errors,
        );
        fill(
            "matchers.subject",
            &mut matchers.subject,
            &outage.check_id,
            &mut errors,
        );
        match matchers.subject_kind {
            Some(kind) if kind != outage.subject_kind => errors.push(FieldError::new(
                "matchers.subject_kind",
                format!("outage {} is not a {}", outage.id, kind.as_str()),
            )),
            _ => matchers.subject_kind = Some(outage.subject_kind),
        }
        into_result(errors)
    }

    /// Checks every rule a new silence follows at `now`, with silences
    /// capped at `max`.
    ///
    /// # Errors
    ///
    /// Every problem found, each at its field path.
    pub fn validate(
        &self,
        now: Timestamp,
        max: Duration,
        known: &Known<'_>,
    ) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        check_text("reason", &self.reason, &mut errors);
        let lasts = self.expires_at.as_millis().saturating_sub(now.as_millis());
        if lasts <= 0 {
            errors.push(FieldError::new("expires_at", "must be in the future"));
        } else if u128::from(lasts.unsigned_abs()) > max.get().as_millis() {
            errors.push(FieldError::new(
                "expires_at",
                format!("must be at most {max} from now (`[server] max_silence`)"),
            ));
        }
        self.check_matchers(known, &mut errors);
        into_result(errors)
    }

    fn check_matchers(&self, known: &Known<'_>, errors: &mut Vec<FieldError>) {
        let matchers = &self.matchers;
        if matchers.is_empty() && self.episode.is_none() {
            errors.push(FieldError::new(
                "matchers",
                "name at least one of node, subject_kind, subject, component and notifier, or an episode: a silence never matches everything",
            ));
        }
        if let Some(node) = &matchers.node
            && (node.is_empty() || node.contains(char::is_whitespace))
        {
            errors.push(FieldError::new(
                "matchers.node",
                "must be non-empty with no whitespace",
            ));
        }
        if let Some(subject) = &matchers.subject {
            match (known.subject)(subject) {
                None => errors.push(FieldError::new(
                    "matchers.subject",
                    format!("no [[check]] or [[heartbeat]] has id {subject:?}"),
                )),
                Some(kind) if matchers.subject_kind.is_some_and(|wanted| wanted != kind) => {
                    errors.push(FieldError::new(
                        "matchers.subject_kind",
                        format!("{subject:?} is a {}", kind.as_str()),
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(component) = &matchers.component
            && !(known.component)(component)
        {
            errors.push(FieldError::new(
                "matchers.component",
                format!("no [[component]] has id {component:?}"),
            ));
        }
        if let Some(notifier) = &matchers.notifier
            && !(known.notifier)(notifier)
        {
            errors.push(FieldError::new(
                "matchers.notifier",
                format!("no [[notifier]] has id {notifier:?}"),
            ));
        }
    }
}

/// Sets `field` to `value`, or reports a matcher naming something else.
fn fill(path: &str, field: &mut Option<String>, value: &str, errors: &mut Vec<FieldError>) {
    match field {
        Some(given) if given != value => errors.push(FieldError::new(
            path,
            format!("the episode's outage is on {value:?}, not {given:?}"),
        )),
        _ => *field = Some(value.to_owned()),
    }
}

text_enum! {
    /// Whether a silence holds back alerts now, and if not, what ended it.
    SilenceState, "state" {
        /// It holds back the alerts it matches.
        Active => "active",
        /// `expires_at` passed.
        Expired => "expired",
        /// Someone ended it early.
        Ended => "ended",
        /// Its episode's outage closed.
        EpisodeEnded => "episode_ended",
    }
}

text_enum! {
    /// Which message a silence held back.
    HeldKind, "kind" {
        /// The alert started, or its health changed.
        Fire => "fire",
        /// The alert was still firing.
        Repeat => "repeat",
        /// The alert ended, before any message for it was sent.
        Resolve => "resolve",
    }
}

/// One message a silence held back: the alert still fired (or resolved)
/// and is recorded; only the sending was skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HeldMessage {
    /// The delivery's id in the outbox.
    pub delivery_id: i64,
    /// Fire, repeat or resolve.
    pub kind: HeldKind,
    /// The alert's health when the message was queued.
    pub health: Health,
    /// When it was held back.
    pub at: Timestamp,
    /// The alert's id.
    pub alert_id: i64,
    /// The node that saw the alert.
    pub node: String,
    /// Check or heartbeat.
    pub subject_kind: CheckKind,
    /// The check or heartbeat id.
    pub subject: String,
    /// The notifier it was for.
    pub notifier: String,
}

/// A silence and what it held back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Silence {
    /// Assigned by tocsin; never reused.
    pub id: i64,
    /// Which alerts it holds back.
    pub matchers: Matchers,
    /// The outage it is scoped to, if any.
    pub episode: Option<i64>,
    /// Why.
    pub reason: String,
    /// When it was made.
    pub created_at: Timestamp,
    /// Who made it: the caller the server authenticated, never a name
    /// the request gave.
    pub created_by: Actor,
    /// When it stops applying.
    pub expires_at: Timestamp,
    /// When someone ended it early, if they did.
    pub ended_at: Option<Timestamp>,
    /// Who ended it early.
    pub ended_by: Option<Actor>,
    /// When its episode's outage closed, if it has.
    pub episode_ended_at: Option<Timestamp>,
    /// Whether it applies now, as of the request.
    pub state: SilenceState,
    /// The messages it held back, newest first: at most the latest 100.
    /// Kept as long as failed deliveries are.
    pub held: Vec<HeldMessage>,
    /// How many messages it held back that are still kept, including any
    /// past the 100 in `held`.
    pub held_total: u32,
}

impl Silence {
    /// Its state at `now`: whichever of ending it early, its episode
    /// closing and `expires_at` came first, or active if none has.
    #[must_use]
    pub fn state_at(&self, now: Timestamp) -> SilenceState {
        [
            self.ended_at.map(|at| (at, SilenceState::Ended)),
            self.episode_ended_at
                .map(|at| (at, SilenceState::EpisodeEnded)),
            (self.expires_at <= now).then_some((self.expires_at, SilenceState::Expired)),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(at, _)| *at)
        .map_or(SilenceState::Active, |(_, state)| state)
    }
}

/// `GET /api/v1/silences`: one page of silences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SilenceList {
    /// Newest first.
    pub silences: Vec<Silence>,
    /// Pass as `before` to get the next page; `null` on the last page.
    pub next_before: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::{Known, Matchers, NewSilence, Silence, SilenceState, Target};
    use crate::Health;
    use crate::config::Duration;
    use crate::incident::{Actor, Timestamp};
    use crate::status::{CheckKind, OutageRecord};

    const NOW: i64 = 1_000_000_000;
    const DAY: Duration = Duration::from_secs(86_400);

    fn known() -> Known<'static> {
        Known {
            subject: &|id| match id {
                "web" => Some(CheckKind::Check),
                "backup" => Some(CheckKind::Heartbeat),
                _ => None,
            },
            component: &|id| id == "site",
            notifier: &|id| id == "pager",
        }
    }

    fn new(expires_in_ms: i64) -> NewSilence {
        NewSilence {
            matchers: Matchers {
                subject: Some("web".to_owned()),
                ..Matchers::default()
            },
            episode: None,
            expires_at: Timestamp::from_millis(NOW + expires_in_ms),
            reason: "deploying".to_owned(),
        }
    }

    fn paths(new: &NewSilence) -> Vec<String> {
        new.validate(Timestamp::from_millis(NOW), DAY, &known())
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|error| error.path)
            .collect()
    }

    #[test]
    fn an_expiry_must_be_ahead_and_within_the_cap() {
        assert_eq!(paths(&new(60_000)), Vec::<String>::new());
        let day_ms = i64::try_from(DAY.get().as_millis()).unwrap();
        assert!(paths(&new(day_ms)).is_empty(), "the cap itself is allowed");
        assert_eq!(paths(&new(day_ms + 1)), ["expires_at"]);
        assert_eq!(paths(&new(0)), ["expires_at"]);
        assert_eq!(paths(&new(-1)), ["expires_at"]);
    }

    #[test]
    fn the_cap_message_names_the_setting() {
        let errors = new(i64::MAX / 2)
            .validate(Timestamp::from_millis(NOW), DAY, &known())
            .unwrap_err();
        assert_eq!(
            errors[0].message,
            "must be at most 1d from now (`[server] max_silence`)"
        );
    }

    #[test]
    fn a_silence_needs_a_reason_and_a_matcher() {
        let mut silence = new(60_000);
        silence.reason = "  ".to_owned();
        silence.matchers = Matchers::default();
        assert_eq!(paths(&silence), ["reason", "matchers"]);
        silence.reason = "ok".to_owned();
        silence.episode = Some(4);
        assert!(paths(&silence).is_empty(), "an episode is enough");
    }

    #[test]
    fn matchers_name_configured_ids() {
        let mut silence = new(60_000);
        silence.matchers = Matchers {
            node: Some("a b".to_owned()),
            subject_kind: Some(CheckKind::Heartbeat),
            subject: Some("web".to_owned()),
            component: Some("nope".to_owned()),
            notifier: Some("nope".to_owned()),
        };
        assert_eq!(
            paths(&silence),
            [
                "matchers.node",
                "matchers.subject_kind",
                "matchers.component",
                "matchers.notifier"
            ]
        );
        silence.matchers.subject = Some("gone".to_owned());
        assert!(paths(&silence).contains(&"matchers.subject".to_owned()));
    }

    #[test]
    fn a_missing_expiry_does_not_parse() {
        let error = serde_json::from_str::<NewSilence>(
            r#"{"matchers": {"subject": "web"}, "reason": "deploying"}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expires_at"), "{error}");
    }

    fn outage(ended: bool) -> OutageRecord {
        OutageRecord {
            id: 7,
            node_id: "node-a".to_owned(),
            check_id: "web".to_owned(),
            subject_kind: CheckKind::Check,
            component_id: None,
            health: Health::Down,
            started_at: Timestamp::from_millis(NOW - 1000),
            ended_at: ended.then_some(Timestamp::from_millis(NOW - 1)),
            ending: None,
        }
    }

    #[test]
    fn an_episode_fills_in_its_outage() {
        let mut silence = new(60_000);
        silence.matchers = Matchers {
            notifier: Some("pager".to_owned()),
            ..Matchers::default()
        };
        silence.episode = Some(7);
        silence.scope_to(&outage(false)).unwrap();
        assert_eq!(
            silence.matchers,
            Matchers {
                node: Some("node-a".to_owned()),
                subject_kind: Some(CheckKind::Check),
                subject: Some("web".to_owned()),
                component: None,
                notifier: Some("pager".to_owned()),
            }
        );
    }

    #[test]
    fn an_episode_must_be_open_and_agree_with_the_matchers() {
        let mut silence = new(60_000);
        silence.matchers.node = Some("node-b".to_owned());
        silence.matchers.subject_kind = Some(CheckKind::Heartbeat);
        let paths: Vec<String> = silence
            .scope_to(&outage(true))
            .unwrap_err()
            .into_iter()
            .map(|error| error.path)
            .collect();
        assert_eq!(paths, ["episode", "matchers.node", "matchers.subject_kind"]);
    }

    #[test]
    fn matchers_compare_every_field_set() {
        let target = Target {
            node: "node-a",
            subject_kind: CheckKind::Check,
            subject: "web",
            component: Some("site"),
            notifier: "pager",
        };
        assert!(Matchers::default().matches(&target));
        let mut matchers = Matchers {
            subject: Some("web".to_owned()),
            component: Some("site".to_owned()),
            ..Matchers::default()
        };
        assert!(matchers.matches(&target));
        matchers.notifier = Some("email".to_owned());
        assert!(!matchers.matches(&target));
        let unassigned = Target {
            component: None,
            ..target
        };
        matchers.notifier = None;
        assert!(!matchers.matches(&unassigned));
    }

    #[test]
    fn the_state_is_whatever_ended_it_first() {
        let mut silence = Silence {
            id: 1,
            matchers: Matchers::default(),
            episode: Some(7),
            reason: "r".to_owned(),
            created_at: Timestamp::from_millis(0),
            created_by: Actor::token("ops"),
            expires_at: Timestamp::from_millis(100),
            ended_at: None,
            ended_by: None,
            episode_ended_at: None,
            state: SilenceState::Active,
            held: Vec::new(),
            held_total: 0,
        };
        let at = Timestamp::from_millis;
        assert_eq!(silence.state_at(at(99)), SilenceState::Active);
        assert_eq!(silence.state_at(at(100)), SilenceState::Expired);
        silence.episode_ended_at = Some(at(150));
        assert_eq!(silence.state_at(at(200)), SilenceState::Expired);
        silence.episode_ended_at = Some(at(50));
        assert_eq!(silence.state_at(at(60)), SilenceState::EpisodeEnded);
        silence.ended_at = Some(at(40));
        assert_eq!(silence.state_at(at(60)), SilenceState::Ended);
    }
}
