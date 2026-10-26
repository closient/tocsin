// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin silence`: hold alert messages back for a while on a running
//! server.

use std::fmt::Write as _;

use reqwest::Method;
use serde_json::Value;
use tocsin_core::config::Duration;
use tocsin_core::incident::Timestamp;
use tocsin_core::silence::{Matchers, NewSilence, Silence, SilenceList};
use tocsin_core::status::CheckKind;

use crate::cli::{SilenceCommand, SubjectKindArg};
use crate::client::Client;
use crate::exit::Exit;
use crate::output::{Failure, Report};

const SILENCES: &str = "api/v1/silences";

pub fn run(command: SilenceCommand) -> Result<Report, Failure> {
    match command {
        SilenceCommand::New {
            server,
            node,
            subject_kind,
            subject,
            component,
            notifier,
            episode,
            expires,
            reason,
        } => {
            let new = NewSilence {
                matchers: Matchers {
                    node,
                    subject_kind: subject_kind.map(|kind| match kind {
                        SubjectKindArg::Check => CheckKind::Check,
                        SubjectKindArg::Heartbeat => CheckKind::Heartbeat,
                    }),
                    subject,
                    component,
                    notifier,
                },
                episode,
                expires_at: match expires {
                    Expires::At(at) => Some(at),
                    Expires::In(_) => None,
                },
                expires_in: match expires {
                    Expires::In(lasts) => Some(lasts),
                    Expires::At(_) => None,
                },
                reason,
            };
            let silence: Silence = Client::new(&server)?.write(&Method::POST, SILENCES, &new)?;
            Ok(written(&silence, "made"))
        }
        SilenceCommand::List {
            server,
            limit,
            before,
        } => {
            let mut path = format!("{SILENCES}?limit={limit}");
            if let Some(before) = before {
                let _ = write!(path, "&before={before}");
            }
            let list: SilenceList = Client::new(&server)?.get_with_token(&path)?;
            Ok(Report {
                exit: Exit::Ok,
                json: to_json(&list),
                text: table(&list),
            })
        }
        SilenceCommand::Show { server, id } => {
            let silence: Silence =
                Client::new(&server)?.get_with_token(&format!("{SILENCES}/{id}"))?;
            Ok(Report {
                exit: Exit::Ok,
                json: to_json(&silence),
                text: describe(&silence),
            })
        }
        SilenceCommand::Expire { server, id } => {
            let id = id.to_string();
            let silence: Silence =
                Client::new(&server)?.post_query(&["api", "v1", "silences", &id, "expire"], &[])?;
            Ok(written(&silence, "ended"))
        }
    }
}

/// When a silence given on the command line ends.
#[derive(Debug, Clone, Copy)]
pub enum Expires {
    /// This long after the server receives it: the server counts, so a
    /// slow request or a skewed clock here does not shorten it.
    In(Duration),
    /// At this instant.
    At(Timestamp),
}

/// A duration forward from now, such as `30m`, or an RFC 3339 instant,
/// for `--expires`.
pub fn parse_expires(text: &str) -> Result<Expires, String> {
    if let Ok(lasts) = Duration::parse(text) {
        return Ok(Expires::In(lasts));
    }
    text.parse::<Timestamp>().map(Expires::At).map_err(|error| {
        format!("{error}; give a duration from now such as 30m or 4h, or RFC 3339 such as 2026-10-23T14:05:00Z")
    })
}

fn to_json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("API types serialize")
}

fn written(silence: &Silence, verb: &str) -> Report {
    Report {
        exit: Exit::Ok,
        json: to_json(silence),
        text: format!(
            "silence {} {verb}: {}, until {}",
            silence.id, silence.state, silence.expires_at
        ),
    }
}

/// What a silence matches, for a person: `subject=api node=a`.
fn matching(silence: &Silence) -> String {
    let matchers = &silence.matchers;
    let mut parts = Vec::new();
    if let Some(episode) = silence.episode {
        parts.push(format!("episode={episode}"));
    }
    for (name, value) in [
        ("node", matchers.node.as_deref()),
        ("subject_kind", matchers.subject_kind.map(CheckKind::as_str)),
        ("subject", matchers.subject.as_deref()),
        ("component", matchers.component.as_deref()),
        ("notifier", matchers.notifier.as_deref()),
    ] {
        if let Some(value) = value {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join(" ")
}

fn table(list: &SilenceList) -> String {
    if list.silences.is_empty() {
        return "no silences".to_owned();
    }
    let mut text = String::new();
    for silence in &list.silences {
        let _ = writeln!(
            text,
            "{:>5}  {:<13}  {}  held {:>3}  {}",
            silence.id,
            silence.state.as_str(),
            silence.expires_at,
            silence.held_total,
            matching(silence),
        );
    }
    if let Some(next) = list.next_before {
        let _ = writeln!(text, "more: --before {next}");
    }
    text.trim_end().to_owned()
}

/// One silence for a person: what it matches, who made and ended it,
/// then what it held back.
fn describe(silence: &Silence) -> String {
    let mut text = format!(
        "silence {}: {}\nmatches {}\nreason: {}\nmade {} by {}\nexpires {}\n",
        silence.id,
        silence.state,
        matching(silence),
        silence.reason,
        silence.created_at,
        silence.created_by,
        silence.expires_at,
    );
    if let (Some(at), Some(by)) = (silence.ended_at, &silence.ended_by) {
        let _ = writeln!(text, "ended {at} by {by}");
    }
    if let Some(at) = silence.episode_ended_at {
        let _ = writeln!(text, "episode ended {at}");
    }
    let _ = writeln!(text, "held back {}", silence.held_total);
    for held in &silence.held {
        let _ = writeln!(
            text,
            "  {}  {:<7}  {:<8}  {}/{} to {}",
            held.at,
            held.kind.as_str(),
            held.health.as_str(),
            held.node,
            held.subject,
            held.notifier
        );
    }
    text.trim_end().to_owned()
}
