// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin notify test`: send one notifier a test alert, then its resolve.

use std::path::Path;

use serde_json::json;
use tocsin_alert::{Alert, AlertEvent, HTTP_TIMEOUT, SendError};
use tocsin_core::Health;
use tocsin_core::config::closest;
use tocsin_store::SubjectKind;

use crate::config::{self, Loaded};
use crate::exit::Exit;
use crate::output::{Failure, Report};

/// The subject id a test alert carries, so receivers can tell it apart.
const TEST_SUBJECT: &str = "tocsin-test";

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
    let built = tocsin_alert::build(notifier, configuration.server.email.as_ref(), &client)
        .map_err(|error| {
            Failure::new(Exit::Runtime, format!("notifier `{notifier_id}`: {error}"))
        })?;
    let node_id = configuration.server.node_id.clone().unwrap_or_default();
    let (fire, resolve) = runtime.block_on(async {
        let started = tocsin_store::now_ms();
        let mut alert = Alert {
            event: AlertEvent::Fire,
            node_id,
            subject_kind: SubjectKind::Check,
            subject_id: TEST_SUBJECT.to_owned(),
            subject_name: "tocsin notify test".to_owned(),
            component_id: None,
            component_name: None,
            health: Health::Down,
            started_at_ms: started,
            ended_at_ms: None,
            sent_at_ms: started,
            delivery_id: None,
            test: true,
        };
        let fire = built.send(&alert).await;
        // The resolve goes whatever happened to the fire: a fire that timed
        // out may still have arrived, and must not be left firing.
        let ended = tocsin_store::now_ms();
        alert.event = AlertEvent::Resolve;
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
