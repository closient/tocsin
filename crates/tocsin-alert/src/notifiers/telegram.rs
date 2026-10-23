// SPDX-License-Identifier: AGPL-3.0-only

//! Telegram, through a bot's `sendMessage`.

use serde_json::json;
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// The Bot API's base URL.
pub const API_BASE: &str = "https://api.telegram.org";

/// Sends messages as a Telegram bot.
#[derive(Debug)]
pub struct Telegram {
    client: reqwest::Client,
    url: String,
    chat_id: String,
    redactor: Redactor,
}

impl Telegram {
    /// A notifier sending to `chat_id` as the bot with `bot_token`, which is
    /// secret, through the Bot API at `api_base`.
    #[must_use]
    pub fn new(client: reqwest::Client, api_base: &str, bot_token: &str, chat_id: String) -> Self {
        let url = format!(
            "{}/bot{bot_token}/sendMessage",
            api_base.trim_end_matches('/')
        );
        // The token is in the URL's path; redact both.
        let redactor = Redactor::new([bot_token, url.as_str()]);
        Telegram {
            client,
            url,
            chat_id,
            redactor,
        }
    }
}

impl Notifier for Telegram {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let body = json!({
                "chat_id": self.chat_id,
                "text": alert.text(),
                "disable_web_page_preview": true,
            });
            let request = self.client.post(&self.url).json(&body);
            self.redactor.send(request).await.map(drop)
        })
    }
}
