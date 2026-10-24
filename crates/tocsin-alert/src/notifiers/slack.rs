// SPDX-License-Identifier: AGPL-3.0-only

//! Slack, through an incoming webhook: one in the configuration, or one
//! connected on the admin page and named by `connection`.

use std::sync::Arc;

use serde_json::{Value, json};
use tocsin_engine::ports::BoxFuture;
use tocsin_store::{SlackHook, Store};

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// Slack's answers meaning the webhook will never work again: the channel
/// archived or deleted, the app removed or its token revoked. Anything
/// else, such as `invalid_payload`, is wrong with one message, not with
/// the webhook.
const REVOKED: &[&str] = &[
    "action_prohibited",
    "channel_is_archived",
    "channel_not_found",
    "invalid_token",
    "no_active_hooks",
    "no_service",
    "no_service_id",
    "no_team",
    "team_disabled",
];

/// Why Slack will never accept a post to this webhook again, if its answer
/// (`status` and `body`) says so: Slack's error code, or the bare status
/// of a webhook that is gone (404 or 410). `None` for success and for
/// every failure that might pass on another try or with another message.
#[must_use]
pub fn revoked(status: reqwest::StatusCode, body: &str) -> Option<String> {
    if status.is_success() {
        return None;
    }
    let code = body.trim();
    if let Some(code) = REVOKED.iter().find(|known| **known == code) {
        return Some((*code).to_owned());
    }
    (status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE)
        .then(|| format!("HTTP {}", status.as_u16()))
}

/// The message an alert posts.
fn payload(alert: &Alert) -> Value {
    let marker = if alert.is_resolve() {
        ":white_check_mark:"
    } else {
        ":rotating_light:"
    };
    json!({
        "text": format!("{marker} {}", alert.title()),
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("{marker} *{}*\n{}", alert.title(), alert.body()),
                },
            },
        ],
    })
}

/// Posts `body` to the secret `url`. `Err(Ok(reason))` when Slack says the
/// webhook is revoked; `Err(Err(_))` for any other failure.
///
/// # Errors
///
/// The post failed; see above.
pub async fn post(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
) -> Result<(), Result<String, SendError>> {
    let redactor = Redactor::new([url]);
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|error| Err(redactor.transport(error)))?;
    let (status, retry_after, text) = Redactor::read(response).await;
    if let Some(reason) = revoked(status, &text) {
        return Err(Ok(reason));
    }
    redactor
        .judge(status, retry_after, &text)
        .map(drop)
        .map_err(Err)
}

/// Posts to a Slack incoming webhook from the configuration.
#[derive(Debug)]
pub struct Slack {
    client: reqwest::Client,
    webhook_url: String,
    redactor: Redactor,
}

impl Slack {
    /// A notifier posting to `webhook_url`, which is secret.
    #[must_use]
    pub fn new(client: reqwest::Client, webhook_url: String) -> Self {
        let redactor = Redactor::new([webhook_url.as_str()]);
        Slack {
            client,
            webhook_url,
            redactor,
        }
    }
}

impl Notifier for Slack {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let request = self.client.post(&self.webhook_url).json(&payload(alert));
            self.redactor.send(request).await.map(drop)
        })
    }
}

/// Where connected Slack channels are kept: the [`Store`] in `tocsin
/// serve`; the database file, read-only, in `tocsin notify test`.
pub trait SlackHooks: Send + Sync {
    /// The connection called `name`.
    fn hook<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<SlackHook, String>>;

    /// Marks connection `id` dead because of `reason`, Slack's answer.
    fn kill<'a>(&'a self, id: i64, reason: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

impl SlackHooks for Store {
    fn hook<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<SlackHook, String>> {
        Box::pin(async move {
            self.slack_hook(name.to_owned())
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn kill<'a>(&'a self, id: i64, reason: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.kill_slack(id, reason.to_owned(), tocsin_store::now_ms())
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
    }
}

/// Posts to the Slack channel connected on the admin page as `name`.
///
/// The webhook URL is looked up for each alert, so a channel reconnected
/// on the admin page is used at once. A name not connected, or one Slack
/// revoked, fails every alert loudly and permanently: the delivery is
/// given up, logged and counted as abandoned, never skipped quietly. When
/// Slack answers that the webhook is revoked, the connection is marked
/// dead, which the admin page and `/metrics` show.
pub struct SlackConnection {
    client: reqwest::Client,
    name: String,
    hooks: Arc<dyn SlackHooks>,
}

impl std::fmt::Debug for SlackConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConnection")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl SlackConnection {
    /// A notifier posting to the connection called `name` in `hooks`.
    #[must_use]
    pub fn new(client: reqwest::Client, name: String, hooks: Arc<dyn SlackHooks>) -> Self {
        SlackConnection {
            client,
            name,
            hooks,
        }
    }
}

impl Notifier for SlackConnection {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let name = &self.name;
            let (id, url) = match self.hooks.hook(name).await {
                Ok(SlackHook::Live { id, url }) => (id, url),
                Ok(SlackHook::Dead { reason }) => {
                    tracing::error!(connection = %name, %reason, "Slack connection is dead; reconnect it on the admin page");
                    return Err(SendError::permanent(format!(
                        "Slack connection `{name}` is dead ({reason}); reconnect it on the admin page"
                    )));
                }
                Ok(SlackHook::Missing) => {
                    tracing::error!(connection = %name, "no Slack connection by this name; connect it on the admin page");
                    return Err(SendError::permanent(format!(
                        "no Slack connection `{name}`; connect it on the admin page"
                    )));
                }
                Err(error) => {
                    return Err(SendError::transient(format!(
                        "cannot read Slack connection `{name}`: {error}"
                    )));
                }
            };
            match post(&self.client, url.expose(), &payload(alert)).await {
                Ok(()) => Ok(()),
                Err(Err(error)) => Err(error),
                Err(Ok(reason)) => {
                    tracing::error!(connection = %name, %reason, "Slack revoked the connection; marking it dead");
                    if let Err(error) = self.hooks.kill(id, &reason).await {
                        tracing::error!(connection = %name, %error, "cannot mark the Slack connection dead");
                    }
                    Err(SendError::permanent(format!(
                        "Slack revoked connection `{name}` ({reason}); reconnect it on the admin page"
                    )))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::revoked;

    #[test]
    fn only_answers_that_end_the_webhook_revoke_it() {
        for (status, body, reason) in [
            (
                StatusCode::NOT_FOUND,
                "channel_not_found",
                Some("channel_not_found"),
            ),
            (
                StatusCode::GONE,
                "channel_is_archived\n",
                Some("channel_is_archived"),
            ),
            (
                StatusCode::FORBIDDEN,
                "invalid_token",
                Some("invalid_token"),
            ),
            (StatusCode::NOT_FOUND, "no_service", Some("no_service")),
            (StatusCode::NOT_FOUND, "<html>gone</html>", Some("HTTP 404")),
            (StatusCode::GONE, "", Some("HTTP 410")),
            (StatusCode::BAD_REQUEST, "invalid_payload", None),
            (StatusCode::BAD_REQUEST, "no_text", None),
            (StatusCode::FORBIDDEN, "", None),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limited", None),
            (StatusCode::INTERNAL_SERVER_ERROR, "", None),
            (StatusCode::OK, "ok", None),
        ] {
            assert_eq!(
                revoked(status, body).as_deref(),
                reason,
                "{status} {body:?}"
            );
        }
    }
}
