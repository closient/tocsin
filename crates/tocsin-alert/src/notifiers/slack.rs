// SPDX-License-Identifier: AGPL-3.0-only

//! Slack, through an incoming webhook.

use serde_json::json;
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// Posts to a Slack incoming webhook.
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
            let marker = if alert.is_resolve() {
                ":white_check_mark:"
            } else {
                ":rotating_light:"
            };
            let body = json!({
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
            });
            let request = self.client.post(&self.webhook_url).json(&body);
            self.redactor.send(request).await.map(drop)
        })
    }
}
