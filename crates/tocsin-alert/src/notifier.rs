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

    /// Stops `alert`'s earlier messages from ringing, for a notifier whose
    /// messages keep ringing until acknowledged (a Pushover emergency).
    /// `alert` is the message that rang. The worker calls it as a delivery
    /// of its own, queued when the alert resolves or stops ringing. Most
    /// notifiers have nothing to cancel.
    ///
    /// # Errors
    ///
    /// As [`Notifier::send`].
    fn cancel<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        let _ = alert;
        Box::pin(async { Ok(()) })
    }
}

/// The longest `Retry-After` honoured; a longer one waits this long.
pub const MAX_RETRY_AFTER: Duration = Duration::from_hours(1);

/// Why a send failed, with every secret redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    /// What went wrong.
    pub message: String,
    /// Whether trying again cannot help (a 4xx other than 408 or 429, or a
    /// notifier that is not configured).
    pub permanent: bool,
    /// How long the destination asked to be left alone (a 429 or 503
    /// `Retry-After`), at most [`MAX_RETRY_AFTER`]. The next attempt waits
    /// this long instead of the backoff.
    pub retry_after: Option<Duration>,
}

impl SendError {
    /// A failure worth retrying.
    #[must_use]
    pub fn transient(message: impl Into<String>) -> Self {
        SendError {
            message: message.into(),
            permanent: false,
            retry_after: None,
        }
    }

    /// A failure retrying cannot fix.
    #[must_use]
    pub fn permanent(message: impl Into<String>) -> Self {
        SendError {
            message: message.into(),
            permanent: true,
            retry_after: None,
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
    /// 408, 429 and 5xx are worth retrying, a 429 or 503 after its
    /// `Retry-After` if it sent one; any other status is permanent.
    ///
    /// # Errors
    ///
    /// The status is not a success.
    pub async fn check(&self, response: reqwest::Response) -> Result<String, SendError> {
        let status = response.status();
        let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| retry_after(value, jiff::Timestamp::now()))
        } else {
            None
        };
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
            SendError {
                retry_after,
                ..SendError::transient(message)
            }
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

/// A `Retry-After` value, delay-seconds or an HTTP-date, as a wait from
/// `now`, at most [`MAX_RETRY_AFTER`]. A date already past waits nothing;
/// a value that is neither is ignored.
fn retry_after(value: &str, now: jiff::Timestamp) -> Option<Duration> {
    let value = value.trim();
    let wait = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        let at = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
        let millis = at.as_millisecond().saturating_sub(now.as_millisecond());
        Duration::from_millis(u64::try_from(millis).unwrap_or(0))
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_takes_seconds_or_an_http_date_and_is_capped() {
        let now: jiff::Timestamp = "2026-10-23T12:00:00Z".parse().unwrap();
        assert_eq!(retry_after("7", now), Some(Duration::from_secs(7)));
        assert_eq!(
            retry_after("Fri, 23 Oct 2026 12:00:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_after("Fri, 23 Oct 2026 11:00:00 GMT", now),
            Some(Duration::ZERO),
            "a date already past"
        );
        assert_eq!(retry_after("86400", now), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after("soon", now), None);
        assert_eq!(retry_after("-5", now), None);
    }

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
