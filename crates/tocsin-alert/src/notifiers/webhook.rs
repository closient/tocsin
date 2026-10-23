// SPDX-License-Identifier: AGPL-3.0-only

//! A generic JSON webhook, signed with HMAC-SHA256.
//!
//! The signature covers `"{timestamp}.{body}"`, where `timestamp` is the
//! `X-Tocsin-Timestamp` header (unix seconds) and `body` is the exact bytes
//! sent, so a receiver can reject a replayed or altered request. See
//! `docs/alerting.md`.

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use tocsin_engine::ports::BoxFuture;

use crate::alert::{Alert, AlertEvent};
use crate::notifier::{Notifier, Redactor, SendError};

/// The payload's version; it changes only when a field changes meaning or
/// goes away.
pub const PAYLOAD_VERSION: u32 = 1;
/// The header carrying the signing time (unix seconds).
pub const TIMESTAMP_HEADER: &str = "X-Tocsin-Timestamp";
/// The header carrying `sha256=<hex>`.
pub const SIGNATURE_HEADER: &str = "X-Tocsin-Signature";

/// Posts signed JSON to a URL.
#[derive(Debug)]
pub struct Webhook {
    client: reqwest::Client,
    url: String,
    signing_secret: String,
    bearer_token: Option<String>,
    redactor: Redactor,
}

impl Webhook {
    /// A notifier posting to `url`, signing with `signing_secret` and
    /// sending `bearer_token` if given; both are secret.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        url: String,
        signing_secret: String,
        bearer_token: Option<String>,
    ) -> Self {
        let redactor = Redactor::new(
            [Some(signing_secret.as_str()), bearer_token.as_deref()]
                .into_iter()
                .flatten(),
        );
        Webhook {
            client,
            url,
            signing_secret,
            bearer_token,
            redactor,
        }
    }
}

/// The JSON a webhook receives.
#[must_use]
pub fn payload(alert: &Alert) -> serde_json::Value {
    json!({
        "version": PAYLOAD_VERSION,
        "event": alert.event.as_str(),
        "status": if alert.event == AlertEvent::Resolve { "resolved" } else { "firing" },
        "identity": alert.identity(),
        "node_id": alert.node_id,
        "subject": {
            "kind": alert.subject_kind.as_str(),
            "id": alert.subject_id,
            "name": alert.subject_name,
        },
        "component_id": alert.component_id,
        "health": alert.health.as_str(),
        "started_at_ms": alert.started_at_ms,
        "ended_at_ms": alert.ended_at_ms,
        "duration_ms": alert.duration_ms(),
        "sent_at_ms": alert.sent_at_ms,
        "delivery_id": alert.delivery_id,
        "test": alert.test,
        "title": alert.title(),
        "text": alert.body(),
    })
}

/// `sha256=<hex>` of HMAC-SHA256 over `"{timestamp}.{body}"`.
///
/// # Panics
///
/// Never: HMAC takes a key of any length.
#[must_use]
pub fn signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

impl Notifier for Webhook {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&payload(alert))
                .map_err(|error| SendError::permanent(error.to_string()))?;
            let timestamp = alert.sent_at_ms.div_euclid(1000);
            let signature = signature(self.signing_secret.as_bytes(), timestamp, &body);
            let mut request = self
                .client
                .post(&self.url)
                .header("Content-Type", "application/json")
                .header(TIMESTAMP_HEADER, timestamp.to_string())
                .header(SIGNATURE_HEADER, signature)
                .header("X-Tocsin-Event", alert.event.as_str());
            if let Some(id) = alert.delivery_id {
                request = request.header("X-Tocsin-Delivery", id.to_string());
            }
            if let Some(token) = &self.bearer_token {
                request = request.bearer_auth(token);
            }
            self.redactor.send(request.body(body)).await.map(drop)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_matches_a_known_vector() {
        // printf '1800000000.{"a":1}' | openssl dgst -sha256 -hmac secret
        assert_eq!(
            signature(b"secret", 1_800_000_000, br#"{"a":1}"#),
            "sha256=440dfbb83aabca24d9e5027629cfd81cb45254690f8d410bb28f1cc17c990dbc"
        );
    }
}
