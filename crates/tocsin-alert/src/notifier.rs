// SPDX-License-Identifier: AGPL-3.0-only

//! The [`Notifier`] trait, and what the HTTP notifiers share: one client,
//! status handling and redaction.

use std::fmt;
use std::time::Duration;

use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;

/// Sends alerts to one destination.
pub trait Notifier: Send + Sync + 'static {
    /// Sends one message. A resolve carries the identity of the fire it
    /// ends.
    ///
    /// # Errors
    ///
    /// The destination refused or could not be reached; see
    /// [`SendError::permanent`].
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>>;
}

/// Why a send failed, with every secret redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    /// What went wrong.
    pub message: String,
    /// Whether trying again cannot help (a 4xx other than 408 or 429, or a
    /// notifier that is not configured).
    pub permanent: bool,
}

impl SendError {
    /// A failure worth retrying.
    #[must_use]
    pub fn transient(message: impl Into<String>) -> Self {
        SendError {
            message: message.into(),
            permanent: false,
        }
    }

    /// A failure retrying cannot fix.
    #[must_use]
    pub fn permanent(message: impl Into<String>) -> Self {
        SendError {
            message: message.into(),
            permanent: true,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SendError {}

/// How long one HTTP request to a notifier may take; the binary builds the
/// shared client with it.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Removes secret values from text before it is logged or stored.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

impl Redactor {
    /// Redacts each of `secrets`, and for a URL, its path and query too.
    #[must_use]
    pub fn new<'a>(secrets: impl IntoIterator<Item = &'a str>) -> Self {
        let mut all = Vec::new();
        for secret in secrets {
            if secret.is_empty() {
                continue;
            }
            all.push(secret.to_owned());
            // A webhook URL's token lives in its path; a server may echo
            // the path alone.
            if let Ok(url) = url::Url::parse(secret) {
                if url.path().len() > 1 {
                    all.push(url.path().to_owned());
                }
                if let Some(query) = url.query() {
                    all.push(query.to_owned());
                }
            }
        }
        // Longest first, so a secret containing another goes whole.
        all.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Redactor { secrets: all }
    }

    /// `text` with every secret replaced by `[redacted]`.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.secrets {
            text = text.replace(secret.as_str(), "[redacted]");
        }
        text
    }

    /// A transport error, with the URL stripped and secrets redacted.
    #[must_use]
    pub fn transport(&self, error: reqwest::Error) -> SendError {
        let error = error.without_url();
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        SendError::transient(self.redact(&message))
    }

    /// Turns a response into success or a [`SendError`]: any 2xx succeeds;
    /// 408, 429 and 5xx are worth retrying; any other status is permanent.
    ///
    /// # Errors
    ///
    /// The status is not a success.
    pub async fn check(&self, response: reqwest::Response) -> Result<String, SendError> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(body);
        }
        let snippet: String = body.chars().take(200).collect();
        let message = self.redact(&format!("HTTP {status}: {}", snippet.trim()));
        let retry = status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        Err(if retry {
            SendError::transient(message)
        } else {
            SendError::permanent(message)
        })
    }

    /// Sends `request`, then checks the response as [`Redactor::check`]
    /// does.
    ///
    /// # Errors
    ///
    /// The request could not be sent, or the status is not a success.
    pub async fn send(&self, request: reqwest::RequestBuilder) -> Result<String, SendError> {
        let response = request
            .send()
            .await
            .map_err(|error| self.transport(error))?;
        self.check(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_webhook_url_and_its_path_are_redacted() {
        let redactor = Redactor::new(["https://hooks.slack.com/services/T0/B0/xyz"]);
        assert_eq!(
            redactor.redact("error posting https://hooks.slack.com/services/T0/B0/xyz"),
            "error posting [redacted]"
        );
        assert_eq!(
            redactor.redact("no route for /services/T0/B0/xyz"),
            "no route for [redacted]"
        );
    }
}
