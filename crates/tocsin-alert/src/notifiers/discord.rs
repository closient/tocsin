// SPDX-License-Identifier: AGPL-3.0-only

//! Discord, through a channel webhook.

use serde_json::json;
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// Discord caps a message's content at this many characters.
const MAX_CONTENT: usize = 2000;

/// Posts to a Discord webhook.
#[derive(Debug)]
pub struct Discord {
    client: reqwest::Client,
    webhook_url: String,
    redactor: Redactor,
}

impl Discord {
    /// A notifier posting to `webhook_url`, which is secret.
    #[must_use]
    pub fn new(client: reqwest::Client, webhook_url: String) -> Self {
        let redactor = Redactor::new([webhook_url.as_str()]);
        Discord {
            client,
            webhook_url,
            redactor,
        }
    }
}

impl Notifier for Discord {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let content: String = format!("**{}**\n{}", alert.title(), alert.body())
                .chars()
                .take(MAX_CONTENT)
                .collect();
            // No mentions: a check name must not be able to ping @everyone.
            let body = json!({
                "content": content,
                "allowed_mentions": { "parse": [] },
            });
            let request = self.client.post(&self.webhook_url).json(&body);
            self.redactor.send(request).await.map(drop)
        })
    }
}
