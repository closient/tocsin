// SPDX-License-Identifier: AGPL-3.0-only

//! Node keys: how one node of a federation proves to another that a push
//! is its own.
//!
//! Each node holds an Ed25519 key pair. The private key comes from the
//! environment (`[server] node_key`); the public keys sit under the node's
//! `[[node]]`, where they are safe to commit. A push is signed over
//! [`push_message`], and the receiver accepts it if any of the sender's
//! listed `public_keys` verifies it, so two keys verify during a rotation.
//! Both keys are written `ed25519:<base64>`: the public key's 32 bytes,
//! and the private key's 32-byte seed.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// What every key's text starts with: the algorithm, so another can be
/// added without guessing what an old key is.
const PREFIX: &str = "ed25519:";

/// The pattern every [`PublicKey`] matches, as published in the JSON
/// Schema: `ed25519:` and 32 bytes of standard, padded base64.
pub const PUBLIC_KEY_PATTERN: &str = "^ed25519:[A-Za-z0-9+/]{43}=$";

/// The first line of every signed message, naming what is signed and the
/// version of its layout.
const PUSH_CONTEXT: &str = "tocsin-push-v1";

/// A node's Ed25519 public key, written `ed25519:<base64 of 32 bytes>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// Parses `ed25519:<base64>`, explaining what is wrong when it is not
    /// a public key.
    ///
    /// # Errors
    ///
    /// Returns a message when `text` is not an Ed25519 public key.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "invalid public key `{text}`: write `ed25519:` and the 32-byte key in base64, as `tocsin node keygen` prints it"
            )
        };
        let encoded = text.strip_prefix(PREFIX).ok_or_else(invalid)?;
        let bytes = STANDARD.decode(encoded).map_err(|_| invalid())?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| invalid())?;
        Ok(PublicKey(bytes))
    }

    /// Whether `signature`, decoded, is this key's signature of `message`.
    fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, &self.0)
            .verify(message, signature)
            .is_ok()
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{PREFIX}{}", STANDARD.encode(self.0))
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        PublicKey::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for PublicKey {
    fn schema_name() -> Cow<'static, str> {
        "PublicKey".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": PUBLIC_KEY_PATTERN,
            "description": "An Ed25519 public key: \"ed25519:\" and 32 bytes of base64, as `tocsin node keygen` prints it."
        })
    }
}

/// A node's private key, which signs its pushes. Its value never appears
/// in `Debug` output or error messages.
pub struct NodeKey {
    pair: Ed25519KeyPair,
    seed: [u8; 32],
}

impl NodeKey {
    /// The key whose 32-byte seed is `seed`, which must come from a
    /// cryptographically secure random source.
    ///
    /// # Panics
    ///
    /// Never: every 32-byte seed is an Ed25519 key.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .expect("every 32-byte seed is an Ed25519 key");
        NodeKey { pair, seed }
    }

    /// Parses `ed25519:<base64 of the 32-byte seed>`.
    ///
    /// # Errors
    ///
    /// Returns a message, which never repeats `text`, when `text` is not a
    /// node key.
    pub fn parse(text: &str) -> Result<Self, String> {
        let seed = text
            .strip_prefix(PREFIX)
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        match seed {
            Some(seed) => Ok(NodeKey::from_seed(seed)),
            // The text is a private key, or meant to be one: not echoed.
            None => Err(
                "not a node key: write `ed25519:` and the 32-byte seed in base64, as `tocsin node keygen` prints it"
                    .to_owned(),
            ),
        }
    }

    /// The public key, for the node's `public_keys`.
    ///
    /// # Panics
    ///
    /// Never: an Ed25519 public key is 32 bytes.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        let bytes = self
            .pair
            .public_key()
            .as_ref()
            .try_into()
            .expect("an Ed25519 public key is 32 bytes");
        PublicKey(bytes)
    }

    /// The private key as [`parse`](Self::parse) reads it. Printed once by
    /// `tocsin node keygen`, for the secret store; never logged.
    #[must_use]
    pub fn expose(&self) -> String {
        format!("{PREFIX}{}", STANDARD.encode(self.seed))
    }

    /// The `Tocsin-Signature` of a push of `body` from store `store` of
    /// node `node`: an Ed25519 signature of [`push_message`], in base64.
    #[must_use]
    pub fn sign(&self, node: &str, store: &str, body: &[u8]) -> String {
        let signature = self.pair.sign(&push_message(node, store, body));
        STANDARD.encode(signature.as_ref())
    }
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// What a push's signature covers: `tocsin-push-v1`, the node id, the
/// store id and the body's SHA-256 in hex, one per line. The node and
/// store are signed so a push cannot be replayed as another's.
#[must_use]
pub fn push_message(node: &str, store: &str, body: &[u8]) -> Vec<u8> {
    let digest = ring::digest::digest(&ring::digest::SHA256, body);
    let mut message = format!("{PUSH_CONTEXT}\n{node}\n{store}\n");
    for byte in digest.as_ref() {
        let _ = write!(message, "{byte:02x}");
    }
    message.into_bytes()
}

/// Whether `signature`, a `Tocsin-Signature` header, signs a push of
/// `body` from store `store` of node `node` with any of `public_keys`, the
/// keys listed under that node.
#[must_use]
pub fn verify(
    public_keys: &[PublicKey],
    node: &str,
    store: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    let message = push_message(node, store, body);
    public_keys
        .iter()
        .any(|key| key.verifies(&message, &signature))
}

#[cfg(test)]
mod tests {
    use super::{NodeKey, PUBLIC_KEY_PATTERN, PublicKey, push_message, verify};

    fn key(byte: u8) -> NodeKey {
        NodeKey::from_seed([byte; 32])
    }

    #[test]
    fn a_push_verifies_against_its_own_node_only() {
        let central = key(1);
        let spoke = key(2);
        let signature = spoke.sign("dmz-1", "store-a", b"{}");
        assert!(verify(
            &[spoke.public_key()],
            "dmz-1",
            "store-a",
            b"{}",
            &signature
        ));
        // Signed by one node's key, checked as another's.
        assert!(!verify(
            &[central.public_key()],
            "dmz-1",
            "store-a",
            b"{}",
            &signature
        ));
        // The node, store and body are each covered.
        let keys = [spoke.public_key()];
        assert!(!verify(&keys, "central", "store-a", b"{}", &signature));
        assert!(!verify(&keys, "dmz-1", "store-b", b"{}", &signature));
        assert!(!verify(&keys, "dmz-1", "store-a", b"[]", &signature));
        assert!(!verify(&keys, "dmz-1", "store-a", b"{}", "not base64"));
        assert!(!verify(&[], "dmz-1", "store-a", b"{}", &signature));
    }

    #[test]
    fn both_keys_verify_during_a_rotation() {
        let old = key(3);
        let new = key(4);
        let listed = [old.public_key(), new.public_key()];
        for signer in [&old, &new] {
            let signature = signer.sign("dmz-1", "store-a", b"batch");
            assert!(verify(&listed, "dmz-1", "store-a", b"batch", &signature));
        }
    }

    #[test]
    fn keys_round_trip_through_their_text() {
        let node = key(5);
        let private = node.expose();
        assert!(private.starts_with("ed25519:"));
        let parsed = NodeKey::parse(&private).expect("a node key");
        assert_eq!(parsed.public_key(), node.public_key());

        let public = node.public_key().to_string();
        assert!(
            regex::Regex::new(PUBLIC_KEY_PATTERN)
                .unwrap()
                .is_match(&public)
        );
        assert_eq!(PublicKey::parse(&public), Ok(node.public_key()));
    }

    #[test]
    fn malformed_keys_are_refused_without_echoing_a_private_one() {
        let refused = NodeKey::parse("").expect_err("empty");
        for text in ["ed25519:", "ed25519:AAAA", "rsa:AAAA", "ed25519:!!!!"] {
            assert!(PublicKey::parse(text).is_err(), "{text}");
            // One message whatever was written, so none of it leaks.
            assert_eq!(NodeKey::parse(text).expect_err(text), refused);
        }
        let short = format!("ed25519:{}", "A".repeat(42));
        assert!(PublicKey::parse(&short).is_err());
        assert!(!format!("{:?}", key(6)).contains(&key(6).expose()));
    }

    #[test]
    fn the_signed_message_names_the_context_node_store_and_digest() {
        let message = String::from_utf8(push_message("dmz-1", "store-a", b"")).unwrap();
        assert_eq!(
            message,
            "tocsin-push-v1\ndmz-1\nstore-a\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
