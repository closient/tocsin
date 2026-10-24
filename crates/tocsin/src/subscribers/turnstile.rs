// SPDX-License-Identifier: AGPL-3.0-only

//! Cloudflare Turnstile: checking the answer a sign-up form carries.

use std::net::IpAddr;

use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;

/// Where Cloudflare checks an answer.
pub const VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// The widget's script, which the form loads when Turnstile is on.
pub const SCRIPT_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/api.js";
/// The origin the widget's script and frame come from, for the CSP.
pub const ORIGIN: &str = "https://challenges.cloudflare.com";
/// The form field the widget fills in.
pub const FIELD: &str = "cf-turnstile-response";
/// The longest answer Cloudflare issues.
const MAX_ANSWER: usize = 2048;

/// Checks answers with Cloudflare.
pub struct Verifier {
    pub(super) site_key: String,
    secret_key: String,
    url: String,
    client: reqwest::Client,
}

/// What Cloudflare said.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A person, by Cloudflare's reckoning.
    Passed,
    /// No answer, a wrong one, or one already used.
    Failed,
    /// Cloudflare could not be asked; the cause, for the log.
    Unreachable(String),
}

#[derive(Deserialize)]
struct Answer {
    success: bool,
}

impl Verifier {
    /// A verifier for the widget `site_key` with `secret_key`, asking
    /// `url` (always [`VERIFY_URL`] outside tests).
    pub fn new(site_key: String, secret_key: String, url: String, client: reqwest::Client) -> Self {
        Verifier {
            site_key,
            secret_key,
            url,
            client,
        }
    }

    /// Whether `answer`, from a client at `ip`, passes.
    pub async fn verify(&self, answer: Option<&str>, ip: IpAddr) -> Verdict {
        let Some(answer) = answer.filter(|answer| !answer.is_empty() && answer.len() <= MAX_ANSWER)
        else {
            return Verdict::Failed;
        };
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("secret", &self.secret_key)
            .append_pair("response", answer)
            .append_pair("remoteip", &ip.to_string())
            .finish();
        let response = match self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            // Without the URL, which carries nothing secret but is noise.
            Err(error) => return Verdict::Unreachable(error.without_url().to_string()),
        };
        let status = response.status();
        if !status.is_success() {
            return Verdict::Unreachable(format!("Cloudflare answered {status}"));
        }
        match response.bytes().await {
            Ok(bytes) => match serde_json::from_slice::<Answer>(&bytes) {
                Ok(Answer { success: true }) => Verdict::Passed,
                Ok(Answer { success: false }) => Verdict::Failed,
                Err(error) => Verdict::Unreachable(format!("Cloudflare's answer: {error}")),
            },
            Err(error) => Verdict::Unreachable(error.without_url().to_string()),
        }
    }
}
