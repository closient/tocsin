// SPDX-License-Identifier: AGPL-3.0-only

//! The secrets in a subscriber's links, and masking addresses for the log.
//!
//! - A **confirmation token** is 32 random bytes, base64url. The store
//!   keeps only its SHA-256, so the database never holds a working link,
//!   and finds the subscriber by that digest.
//! - A **manage token** is `<id>.<secret>`: the subscriber's row id and 32
//!   random bytes, base64url. The id finds the row; the secret is compared
//!   in constant time.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest as sha256};
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;

/// 32 random bytes, base64url without padding: 43 characters.
///
/// # Errors
///
/// The operating system's random source failed.
pub fn random() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the system random source failed".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The hex SHA-256 of `token`, as the store keeps a confirmation token.
pub fn digest(token: &str) -> String {
    sha256(&SHA256, token.as_bytes()).as_ref().iter().fold(
        String::with_capacity(64),
        |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )
}

/// The SHA-256 of a normalized address, which the per-address rate limit
/// counts under, so the limiter never holds an address.
pub fn address_key(email: &str) -> [u8; 32] {
    let mut key = [0_u8; 32];
    key.copy_from_slice(sha256(&SHA256, email.as_bytes()).as_ref());
    key
}

/// The manage token for subscriber `id`.
pub fn manage(id: i64, secret: &str) -> String {
    format!("{id}.{secret}")
}

/// Splits a manage token into the subscriber id and the secret; `None`
/// when it is not one.
pub fn parse_manage(token: &str) -> Option<(i64, &str)> {
    let (id, secret) = token.split_once('.')?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) || secret.is_empty() {
        return None;
    }
    Some((id.parse().ok()?, secret))
}

/// Whether `given` is `expected`, in time that does not depend on where
/// they differ.
pub fn same_secret(given: &str, expected: &str) -> bool {
    given.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// `alice@example.com` as `a***@example.com`, for the log: enough to tell
/// a support request apart, never the whole address.
pub fn mask(email: &str) -> String {
    match email.rsplit_once('@') {
        Some((local, domain)) => {
            let first = local.chars().next().map(String::from).unwrap_or_default();
            format!("{first}***@{domain}")
        }
        None => "***".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_url_safe() {
        let one = random().unwrap();
        let two = random().unwrap();
        assert_eq!(one.len(), 43);
        assert_ne!(one, two);
        assert!(
            one.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_eq!(digest(&one).len(), 64);
        assert_eq!(digest(&one), digest(&one));
        assert_ne!(digest(&one), digest(&two));
    }

    #[test]
    fn a_manage_token_round_trips_and_rejects_junk() {
        let token = manage(42, "secret");
        assert_eq!(parse_manage(&token), Some((42, "secret")));
        for junk in [
            "",
            "42",
            "42.",
            ".secret",
            "-1.secret",
            "4x.secret",
            "99999999999999999999.s",
        ] {
            assert_eq!(parse_manage(junk), None, "{junk:?}");
        }
        assert!(same_secret("secret", "secret"));
        assert!(!same_secret("secret", "secreT"));
        assert!(!same_secret("secret", "secret2"));
    }

    #[test]
    fn masking_keeps_the_first_letter_and_the_domain() {
        assert_eq!(mask("alice@example.com"), "a***@example.com");
        assert_eq!(mask("nobody"), "***");
    }
}
