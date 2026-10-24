// SPDX-License-Identifier: AGPL-3.0-only

//! Email over SMTP, through the shared [`Mailer`].
//!
//! Every message about one alert carries the same `References`, so mail
//! clients thread the fire, its repeats and its resolve together.

use lettre::address::Envelope;
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::mail::{Mailer, Outgoing, mailbox, token};
use crate::notifier::{Notifier, SendError};

/// Sends alerts by email.
pub struct Email {
    mailer: Mailer,
    to: Vec<String>,
    envelope: Envelope,
}

impl std::fmt::Debug for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email")
            .field("mailer", &self.mailer)
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl Email {
    /// A notifier sending from `from`, with replies to `reply_to`, to each
    /// of `to`, through the server at `smtp_url` (secret). See
    /// [`Mailer::new`].
    ///
    /// # Errors
    ///
    /// An address is not one, From or Reply-To is an address nobody reads
    /// (`noreply@` and the like: every alert must be answerable), or the
    /// URL is not a usable SMTP URL. The message never repeats the URL.
    pub fn new(smtp_url: &str, from: &str, reply_to: &str, to: &[String]) -> Result<Self, String> {
        let mailer = Mailer::new(smtp_url, from, reply_to)?;
        if to.is_empty() {
            return Err("an email notifier needs at least one `to` address".to_owned());
        }
        let envelope = mailer.envelope(to)?;
        let to = to
            .iter()
            .map(|text| mailbox(text))
            .collect::<Result<_, _>>()?;
        Ok(Email {
            mailer,
            to,
            envelope,
        })
    }

    /// The whole message, CRLF line endings throughout.
    #[must_use]
    pub fn message(&self, alert: &Alert) -> String {
        let domain = self.mailer.domain();
        let identity = token(&alert.identity());
        let thread = format!("<alert.{identity}.{}@{domain}>", alert.started_at_ms);
        let message_id = match alert.delivery_id {
            Some(delivery) => format!("<delivery.{delivery}.{identity}@{domain}>"),
            None => format!(
                "<test.{}.{identity}.{}@{domain}>",
                alert.event.as_str(),
                alert.sent_at_ms,
            ),
        };
        self.mailer.compose(&Outgoing {
            to: self.to.join(", "),
            subject: alert.title(),
            sent_at_ms: alert.sent_at_ms,
            message_id,
            headers: vec![
                ("References", thread.clone()),
                ("In-Reply-To", thread),
                ("X-Tocsin-Identity", identity),
                ("X-Tocsin-Event", alert.event.as_str().to_owned()),
            ],
            body: format!("{}\n\n{}\n", alert.title(), alert.body()),
        })
    }
}

impl Notifier for Email {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let message = self.message(alert);
            self.mailer.send(&self.envelope, &message).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_email_notifier_needs_a_recipient() {
        let error = Email::new(
            "smtp://127.0.0.1:25?tls=none",
            "status@example.com",
            "ops@example.com",
            &[],
        )
        .expect_err("no recipients");
        assert!(error.contains("at least one"), "{error}");
    }
}
