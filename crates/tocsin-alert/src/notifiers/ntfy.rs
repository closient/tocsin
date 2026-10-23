// SPDX-License-Identifier: AGPL-3.0-only

//! ntfy: a plain-text POST to a topic URL.

use tocsin_core::Health;
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// Posts to an ntfy topic.
#[derive(Debug)]
pub struct Ntfy {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
    redactor: Redactor,
}

impl Ntfy {
    /// A notifier posting to the topic at `url`, with an access token for a
    /// protected topic.
    #[must_use]
    pub fn new(client: reqwest::Client, url: String, token: Option<String>) -> Self {
        let redactor = Redactor::new(token.as_deref());
        Ntfy {
            client,
            url,
            token,
            redactor,
        }
    }
}

impl Notifier for Ntfy {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            // ntfy priorities run 1 (min) to 5 (urgent); 3 is the default.
            let (priority, tags) = match (alert.is_resolve(), alert.health) {
                (true, _) => ("3", "white_check_mark"),
                (false, Health::Down) => ("4", "rotating_light"),
                (false, _) => ("3", "warning"),
            };
            let mut request = self
                .client
                .post(&self.url)
                .header("Title", alert.title())
                .header("Priority", priority)
                .header("Tags", tags)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(alert.body());
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            self.redactor.send(request).await.map(drop)
        })
    }
}
