// SPDX-License-Identifier: AGPL-3.0-only

//! Email over SMTP.
//!
//! tocsin writes the message itself (plain text, base64, with an RFC 2047
//! subject) and hands lettre the bytes: lettre's message builder pulls in a
//! dependency whose license is not on tocsin's allowlist. Every message
//! about one alert carries the same `References`, so mail clients thread
//! the fire, its repeats and its resolve together.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use tocsin_core::config::{email_address, is_unanswerable};
use tocsin_engine::ports::BoxFuture;

use crate::alert::{Alert, one_line};
use crate::notifier::{HTTP_TIMEOUT, Notifier, Redactor, SendError};

/// Sends alerts by email.
pub struct Email {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
    reply_to: String,
    to: Vec<String>,
    envelope: lettre::address::Envelope,
    domain: String,
    redactor: Redactor,
}

impl std::fmt::Debug for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email")
            .field("from", &self.from)
            .field("reply_to", &self.reply_to)
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl Email {
    /// A notifier sending from `from`, with replies to `reply_to`, to each
    /// of `to`, through the server at `smtp_url` (secret).
    ///
    /// `smtps://` connects with TLS; `smtp://` requires STARTTLS unless the
    /// URL ends `?tls=none`, which sends in the clear and is meant for a
    /// relay on the same host. User and password come from the URL.
    ///
    /// # Errors
    ///
    /// An address is not one, From or Reply-To is an address nobody reads
    /// (`noreply@` and the like: every alert must be answerable), or the
    /// URL is not a usable SMTP URL. The message never repeats the URL.
    pub fn new(smtp_url: &str, from: &str, reply_to: &str, to: &[String]) -> Result<Self, String> {
        let from_header = sender(from, "from")?;
        let reply_to_header = sender(reply_to, "reply_to")?;
        if to.is_empty() {
            return Err("an email notifier needs at least one `to` address".to_owned());
        }
        let recipients = to
            .iter()
            .map(|text| {
                email_address(text)?
                    .parse::<Address>()
                    .map_err(|error| format!("`{text}`: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sender: Address = email_address(from)?
            .parse()
            .map_err(|error| format!("`{from}`: {error}"))?;
        let domain = sender.domain().to_owned();
        let envelope = lettre::address::Envelope::new(Some(sender), recipients)
            .map_err(|error| error.to_string())?;
        let to = to
            .iter()
            .map(|text| mailbox(text))
            .collect::<Result<_, _>>()?;
        Ok(Email {
            transport: transport(smtp_url)?,
            from: from_header,
            reply_to: reply_to_header,
            to,
            envelope,
            domain,
            redactor: Redactor::new([smtp_url]),
        })
    }

    /// The whole message, CRLF line endings throughout.
    #[must_use]
    pub fn message(&self, alert: &Alert) -> String {
        let mut message = String::new();
        let identity = token(&alert.identity());
        let thread = format!("<alert.{identity}.{}@{}>", alert.started_at_ms, self.domain);
        let id = match alert.delivery_id {
            Some(delivery) => format!("<delivery.{delivery}.{identity}@{}>", self.domain),
            None => format!(
                "<test.{}.{identity}.{}@{}>",
                alert.event.as_str(),
                alert.sent_at_ms,
                self.domain
            ),
        };
        let date = jiff::Timestamp::from_millisecond(alert.sent_at_ms)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .strftime("%a, %d %b %Y %H:%M:%S +0000");
        let headers = [
            ("From", self.from.clone()),
            ("Reply-To", self.reply_to.clone()),
            ("To", self.to.join(", ")),
            ("Subject", encoded_words(&alert.title())),
            ("Date", date.to_string()),
            ("Message-ID", id),
            ("References", thread.clone()),
            ("In-Reply-To", thread),
            ("X-Tocsin-Identity", identity),
            ("X-Tocsin-Event", alert.event.as_str().to_owned()),
            ("Auto-Submitted", "auto-generated".to_owned()),
            ("MIME-Version", "1.0".to_owned()),
            ("Content-Type", "text/plain; charset=utf-8".to_owned()),
            ("Content-Transfer-Encoding", "base64".to_owned()),
        ];
        for (name, value) in headers {
            let _ = write!(message, "{name}: {value}\r\n");
        }
        message.push_str("\r\n");
        let body = format!("{}\n\n{}\n", alert.title(), alert.body()).replace('\n', "\r\n");
        let encoded = STANDARD.encode(body);
        let mut rest = encoded.as_str();
        while !rest.is_empty() {
            let (line, tail) = rest.split_at(rest.len().min(76));
            message.push_str(line);
            message.push_str("\r\n");
            rest = tail;
        }
        message
    }
}

impl Notifier for Email {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let message = self.message(alert);
            match self
                .transport
                .send_raw(&self.envelope, message.as_bytes())
                .await
            {
                Ok(_) => Ok(()),
                Err(error) => {
                    let text = self.redactor.redact(&error.to_string());
                    Err(if error.is_permanent() {
                        SendError::permanent(text)
                    } else {
                        SendError::transient(text)
                    })
                }
            }
        })
    }
}

/// The transport `smtp_url` describes.
fn transport(smtp_url: &str) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let url =
        url::Url::parse(smtp_url).map_err(|error| format!("the SMTP URL is not a URL: {error}"))?;
    let host = url
        .host_str()
        .ok_or("the SMTP URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let tls_none = url
        .query_pairs()
        .any(|(key, value)| key == "tls" && value == "none");
    let builder = match url.scheme() {
        "smtps" => AsyncSmtpTransport::<Tokio1Executor>::relay(&host),
        "smtp" if tls_none => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            host.clone(),
        )),
        "smtp" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host),
        other => {
            return Err(format!(
                "the SMTP URL's scheme is `{other}`, not smtp or smtps"
            ));
        }
    }
    .map_err(|error| format!("the SMTP server's TLS settings: {error}"))?;
    let mut builder = builder.timeout(Some(HTTP_TIMEOUT));
    if let Some(port) = url.port() {
        builder = builder.port(port);
    }
    if !url.username().is_empty() {
        let decode = |text: &str| {
            percent_decode(text).ok_or("the SMTP URL's user or password is not valid UTF-8")
        };
        let user = decode(url.username())?;
        let password = decode(url.password().unwrap_or_default())?;
        builder = builder.credentials(Credentials::new(user, password));
    }
    Ok(builder.build())
}

/// Percent-decodes a URL's user or password.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The header value for a From or Reply-To address, refusing one nobody
/// reads: every alert must be answerable.
fn sender(text: &str, key: &str) -> Result<String, String> {
    let address = email_address(text)?;
    if is_unanswerable(address) {
        return Err(format!(
            "the email {key} address `{address}` is one nobody reads; alerts must be answerable"
        ));
    }
    mailbox(text)
}

/// A `From`, `Reply-To` or `To` value: `"Name" <address>` or the address,
/// with a non-ASCII name encoded.
fn mailbox(text: &str) -> Result<String, String> {
    let address = email_address(text)?;
    if !address.is_ascii() || address.contains(['<', '>', '"', ',']) {
        return Err(format!("`{text}` is not an address tocsin can send to"));
    }
    let name = text
        .rfind('<')
        .map(|open| {
            one_line(text[..open].trim())
                .trim()
                .trim_matches('"')
                .to_owned()
        })
        .unwrap_or_default();
    Ok(if name.is_empty() {
        address.to_owned()
    } else if name.is_ascii() {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\" <{address}>")
    } else {
        format!("{} <{address}>", encoded_words(&name))
    })
}

/// `text` as RFC 2047 base64 encoded-words, folded so no line passes 76
/// characters; plain ASCII without control characters stays as it is.
fn encoded_words(text: &str) -> String {
    let text = one_line(text);
    if text.is_ascii() && text.len() <= 900 {
        return text;
    }
    // 45 bytes encode to 60 characters; with `=?utf-8?B?` and `?=` a word
    // is 72.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for character in text.chars() {
        if chunk.len() + character.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|word| format!("=?utf-8?B?{}?=", STANDARD.encode(word)))
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// `text` reduced to characters safe inside a message id.
fn token(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_noreply_sender_or_reply_to_is_refused() {
        let to = ["ops@example.com".to_owned()];
        for (from, reply_to) in [
            ("NoReply@example.com", "ops@example.com"),
            (
                "Status <status@example.com>",
                "No-Reply <no-reply@example.com>",
            ),
            ("DoNotReply@example.com", "ops@example.com"),
        ] {
            let error = Email::new("smtp://127.0.0.1:25?tls=none", from, reply_to, &to)
                .expect_err("an unanswerable sender is refused");
            assert!(error.contains("nobody reads"), "{error}");
        }
        Email::new(
            "smtp://127.0.0.1:25?tls=none",
            "Status <status@example.com>",
            "ops@example.com",
            &to,
        )
        .expect("answerable addresses are fine");
    }

    #[test]
    fn a_bad_smtp_url_is_reported_without_its_password() {
        let to = ["ops@example.com".to_owned()];
        let error = Email::new(
            "ftp://user:hunter2@mail.example.com",
            "status@example.com",
            "ops@example.com",
            &to,
        )
        .expect_err("ftp is not smtp");
        assert!(!error.contains("hunter2"), "{error}");
    }

    #[test]
    fn names_and_subjects_are_encoded_when_they_are_not_ascii() {
        assert_eq!(
            mailbox("Status <status@example.com>").unwrap(),
            "\"Status\" <status@example.com>"
        );
        assert_eq!(mailbox("ops@example.com").unwrap(), "ops@example.com");
        assert_eq!(
            mailbox("Statüs <status@example.com>").unwrap(),
            "=?utf-8?B?U3RhdMO8cw==?= <status@example.com>"
        );
        assert_eq!(encoded_words("plain"), "plain");
        assert_eq!(
            percent_decode("p%40ss%2Fword").as_deref(),
            Some("p@ss/word")
        );
    }
}
