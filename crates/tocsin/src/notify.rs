// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin notify test`: send one notifier a test alert, then its resolve.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tocsin_alert::{Alert, AlertEvent, HTTP_TIMEOUT, SendError, SlackHooks};
use tocsin_core::Health;
use tocsin_core::config::{AlertingRule, Config, closest};
use tocsin_engine::ports::BoxFuture;
use tocsin_store::{DB_FILE, SlackHook, SubjectKind};

use crate::config::{self, Loaded};
use crate::exit::Exit;
use crate::output::{Failure, Report};

/// The subject id a test alert carries, so receivers can tell it apart.
const TEST_SUBJECT: &str = "tocsin-test";

/// The test alert's fire, at `started`.
fn test_alert(configuration: &Config, started: i64) -> Alert {
    let alerting = configuration.alerting.as_ref();
    Alert {
        event: AlertEvent::Fire,
        node_id: configuration.server.node_id.clone().unwrap_or_default(),
        subject_kind: SubjectKind::Check,
        subject_id: TEST_SUBJECT.to_owned(),
        subject_name: "tocsin notify test".to_owned(),
        labels: required(alerting.map(|alerting| &alerting.labels)),
        annotations: required(alerting.map(|alerting| &alerting.annotations)),
        component_id: None,
        component_name: None,
        health: Health::Down,
        started_at_ms: started,
        ended_at_ms: None,
        ended_by: None,
        sent_at_ms: started,
        delivery_id: None,
        test: true,
    }
}

/// What a test alert sets for each name `[alerting]` requires, so it routes
/// as a real alert would: the first value the contract allows, or `test`
/// when it allows any.
fn required(rules: Option<&BTreeMap<String, AlertingRule>>) -> BTreeMap<String, String> {
    rules
        .into_iter()
        .flatten()
        .filter(|(_, rule)| rule.required)
        .map(|(name, rule)| {
            let value = rule.values.first().map_or("test", String::as_str);
            (name.clone(), value.to_owned())
        })
        .collect()
}

/// `tocsin notify test`.
pub fn test(notifier_id: &str, folder: &Path) -> Result<Report, Failure> {
    let Loaded {
        config: configuration,
        ..
    } = config::load(folder)?;
    let Some(notifier) = configuration
        .notifiers
        .iter()
        .find(|notifier| notifier.id.as_str() == notifier_id)
    else {
        let ids = configuration
            .notifiers
            .iter()
            .map(|notifier| notifier.id.as_str());
        let hint = match closest(notifier_id, ids) {
            Some(near) => format!("; did you mean `{near}`?"),
            None if configuration.notifiers.is_empty() => {
                "; the configuration defines no notifiers".to_owned()
            }
            None => String::new(),
        };
        return Err(Failure::new(
            Exit::InvalidInput,
            format!("no notifier `{notifier_id}`{hint}"),
        ));
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    let client = crate::net::http_client(HTTP_TIMEOUT)
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot set up HTTP: {error}")))?;
    let hooks = ReadOnlyHooks::of(&configuration, folder);
    let built = tocsin_alert::build(
        notifier,
        configuration.server.email.as_ref(),
        &client,
        Some(&hooks),
    )
    .map_err(|error| Failure::new(Exit::Runtime, format!("notifier `{notifier_id}`: {error}")))?;
    let (fire, resolve) = runtime.block_on(async {
        let mut alert = test_alert(&configuration, tocsin_store::now_ms());
        let fire = built.send(&alert).await;
        // The resolve goes whatever happened to the fire: a fire that timed
        // out may still have arrived, and must not be left firing.
        let ended = tocsin_store::now_ms();
        alert.event = AlertEvent::Resolve;
        alert.ended_by = Some(tocsin_store::EndedBy::Recovery);
        alert.ended_at_ms = Some(ended);
        alert.sent_at_ms = ended;
        let resolve = built.send(&alert).await;
        (fire, resolve)
    });
    let outcome = |result: &Result<(), SendError>| match result {
        Ok(()) => json!({ "sent": true }),
        Err(error) => json!({ "sent": false, "error": error.message }),
    };
    let line = |event: &str, result: &Result<(), SendError>| match result {
        Ok(()) => format!("{event}: sent\n"),
        Err(error) => format!("{event}: failed: {}\n", error.message),
    };
    let json = json!({
        "notifier": notifier_id,
        "type": notifier.notifier_type.as_str(),
        "fire": outcome(&fire),
        "resolve": outcome(&resolve),
    });
    let text = format!(
        "notifier `{notifier_id}` ({})\n{}{}",
        notifier.notifier_type.as_str(),
        line("fire", &fire),
        line("resolve", &resolve)
    );
    if fire.is_ok() && resolve.is_ok() {
        Ok(Report {
            json,
            text,
            exit: Exit::Ok,
        })
    } else {
        Err(Failure::new(
            Exit::Runtime,
            text.trim_end().replace('\n', "; "),
        ))
    }
}

/// The Slack connections in the database `tocsin serve` keeps, read without
/// writing: a test never changes what the server runs on, so a connection
/// Slack revoked is reported, and marked dead by the server's next post.
struct ReadOnlyHooks {
    /// The database file, when `[server] data_dir` names its folder.
    database: Option<PathBuf>,
}

impl ReadOnlyHooks {
    /// The connections of the server `configuration` in `folder` sets up.
    fn of(configuration: &tocsin_core::config::Config, folder: &Path) -> Arc<dyn SlackHooks> {
        Arc::new(ReadOnlyHooks {
            database: configuration
                .server
                .data_dir
                .as_ref()
                .map(|data_dir| folder.join(data_dir).join(DB_FILE)),
        })
    }
}

impl SlackHooks for ReadOnlyHooks {
    fn hook<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<SlackHook, String>> {
        Box::pin(async move {
            let Some(path) = &self.database else {
                return Err("no data folder: set `data_dir` in `[server]`".to_owned());
            };
            if !path.exists() {
                return Ok(SlackHook::Missing);
            }
            tocsin_store::inspect_slack_hook(path, name).map_err(|error| error.to_string())
        })
    }

    fn kill<'a>(&'a self, _id: i64, _reason: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async {
            Err("`tocsin notify test` does not change the database; \
                 `tocsin serve` marks the connection dead on its next post"
                .to_owned())
        })
    }
}
