// SPDX-License-Identifier: AGPL-3.0-only

//! A capture [`Outbox`] for tests: it keeps every message instead of
//! sending it, and can hold a send open as a server that never answers.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tocsin_alert::SendError;
use tokio::sync::Notify;

use super::Outbox;

/// One captured message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The envelope recipient.
    pub to: String,
    /// The whole message.
    pub message: String,
}

impl Sent {
    /// The decoded plain-text body.
    pub fn body(&self) -> String {
        use base64::Engine as _;
        let (_, encoded) = self
            .message
            .split_once("\r\n\r\n")
            .expect("headers end with a blank line");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.replace("\r\n", ""))
            .expect("the body is base64");
        String::from_utf8(bytes).expect("the body is UTF-8")
    }

    /// The value of header `name`, if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        let (headers, _) = self.message.split_once("\r\n\r\n")?;
        headers.split("\r\n").find_map(|line| {
            let (key, value) = line.split_once(": ")?;
            key.eq_ignore_ascii_case(name).then_some(value)
        })
    }
}

/// Keeps every message. Send number `hang_on` (1-based), if set, is
/// recorded and then never answers, as a server that took the message and
/// died.
#[derive(Default)]
pub struct Capture {
    sent: Mutex<Vec<Sent>>,
    hang_on: Option<usize>,
    changed: Notify,
}

impl Capture {
    /// A capture that answers every send.
    pub fn new() -> Arc<Self> {
        Arc::new(Capture::default())
    }

    /// A capture whose `nth` send never answers.
    pub fn hanging_on(nth: usize) -> Arc<Self> {
        Arc::new(Capture {
            hang_on: Some(nth),
            ..Capture::default()
        })
    }

    /// Everything captured so far.
    pub fn sent(&self) -> Vec<Sent> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until at least `count` messages are captured; panics after
    /// five seconds.
    pub async fn wait_for(&self, count: usize) -> Vec<Sent> {
        self.wait_until(&format!("{count} messages"), |sent| sent.len() >= count)
            .await
    }

    /// Waits until `done` holds of everything captured; panics, naming
    /// `what`, after five seconds.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&[Sent]) -> bool) -> Vec<Sent> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let changed = self.changed.notified();
            let sent = self.sent();
            if done(&sent) {
                return sent;
            }
            assert!(
                tokio::time::timeout_at(deadline, changed).await.is_ok(),
                "waited for {what}, got {}: {sent:#?}",
                sent.len()
            );
        }
    }

    /// What was captured for `to`, in order.
    pub fn to(&self, to: &str) -> Vec<Sent> {
        self.sent()
            .into_iter()
            .filter(|sent| sent.to == to)
            .collect()
    }
}

impl Outbox for Capture {
    fn send<'a>(&'a self, to: &'a str, message: &'a str) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let count = {
                let mut sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
                sent.push(Sent {
                    to: to.to_owned(),
                    message: message.to_owned(),
                });
                sent.len()
            };
            self.changed.notify_waiters();
            if self.hang_on == Some(count) {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}
