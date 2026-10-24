// SPDX-License-Identifier: AGPL-3.0-only

//! `[slack]`: connecting a Slack channel by signing in to Slack, through
//! the operator's own Slack app. `docs/alerting.md` and
//! `docs/subscribers.md` describe it for operators.
//!
//! - **Sign-in** ([`SlackApp::begin`]) mints a random `state` for Slack's
//!   consent screen and a random nonce for a cookie. Only their digests
//!   are kept, with what is being connected and an expiry ten minutes on.
//! - **The callback** ([`SlackApp::finish`]) takes the state's record out,
//!   so it works once, and accepts it only before it expires and from the
//!   browser holding the nonce: a forged state finds no record, a replayed
//!   one finds it gone, and one carried to another browser lacks the
//!   cookie. Only then is Slack's code exchanged ([`SlackApp::exchange`]).
//! - **Secrets**: the incoming webhook's URL Slack hands back is kept in
//!   the store, read only to post, and never logged, shown or exported.
//!   Neither is the code, nor Slack's answer, of which only the error code
//!   is ever repeated.
//!
//! Records live in memory: a restart in the middle of a sign-in fails it,
//! and the person signs in again.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};
use tocsin_core::config::{Config, MAX_APP_NAME};
use tocsin_store::{Preferences, WebhookUrl};

use crate::exit::Exit;
use crate::output::{Failure, Report};
use crate::subscribers::{limit, token};

/// Where Slack asks a person to approve the app.
pub const AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";
/// Where a code is exchanged for the webhook.
pub const ACCESS_URL: &str = "https://slack.com/api/oauth.v2.access";
/// The one scope the app asks for: a webhook posting to one channel.
pub const SCOPE: &str = "incoming-webhook";
/// Where Slack sends people back, under `[server] public_url`.
pub const CALLBACK: &str = "/slack/callback";
/// Where the Slack app is created from a manifest.
pub const CREATE_APP_URL: &str = "https://api.slack.com/apps?new_app=1&manifest_json=";
/// The cookie holding a sign-in's nonce. `__Host-` makes browsers keep it
/// to this exact host, over https, for every path.
pub const COOKIE: &str = "__Host-tocsin-slack";
/// How long a sign-in may take.
const WITHIN: Duration = Duration::from_mins(10);
/// Sign-ins under way at once, at most. Each is a few hundred bytes; the
/// cap keeps someone starting them in a loop from growing memory.
const MAX_PENDING: usize = 1024;
/// The longest code or state taken from Slack's redirect.
const MAX_PARAMETER: usize = 512;

/// What a sign-in connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A channel for alerts, which `[notifier.slack] connection = name`
    /// posts to; started on the admin page by `admin`.
    Operator {
        /// The connection's name.
        name: String,
        /// The admin who started it.
        admin: String,
    },
    /// A visitor's channel, hearing what `preferences` choose.
    Subscriber {
        /// What it hears about.
        preferences: Preferences,
    },
}

struct Pending {
    purpose: Purpose,
    nonce_digest: String,
    expires: Instant,
}

/// A sign-in started: send the browser to `location` with `cookie` set.
#[derive(Debug)]
pub struct Started {
    /// Slack's consent screen.
    pub location: String,
    /// The `Set-Cookie` value binding the sign-in to this browser.
    pub cookie: String,
}

/// Why a callback was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The state is not one this server issued, was used already, or
    /// expired.
    Unknown,
    /// The state was issued, but to another browser.
    WrongBrowser,
}

/// The channel Slack connected.
#[derive(Debug)]
pub struct Granted {
    /// The incoming webhook. Secret.
    pub webhook_url: WebhookUrl,
    /// The channel's name, as Slack gives it.
    pub channel: String,
    /// The channel's id.
    pub channel_id: Option<String>,
    /// Where the workspace's admins manage the webhook.
    pub configuration_url: Option<String>,
    /// The workspace's name.
    pub team: String,
    /// The workspace's id.
    pub team_id: Option<String>,
}

/// The running `[slack]` app: present only when `[slack]` is.
pub struct SlackApp {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    authorize_url: String,
    access_url: String,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, Pending>>,
    per_ip: Option<limit::Window<IpAddr>>,
}

impl std::fmt::Debug for SlackApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackApp")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl SlackApp {
    /// The app `config` describes; `None` without `[slack]`.
    ///
    /// # Errors
    ///
    /// `[server] public_url` is missing, which config validation refuses
    /// first, or no HTTP client can be built.
    pub fn from_config(config: &Config) -> Result<Option<Self>, String> {
        if config.slack.is_none() {
            return Ok(None);
        }
        Self::with_urls(config, AUTHORIZE_URL, ACCESS_URL).map(Some)
    }

    /// The app sending people to `authorize_url` and exchanging codes at
    /// `access_url`: Slack's outside tests, a fake in them.
    ///
    /// # Errors
    ///
    /// As [`SlackApp::from_config`], or `[slack]` is missing.
    pub fn with_urls(
        config: &Config,
        authorize_url: &str,
        access_url: &str,
    ) -> Result<Self, String> {
        let slack = config.slack.as_ref().ok_or("no [slack] table")?;
        let http = crate::net::http_client(tocsin_alert::HTTP_TIMEOUT)
            .build()
            .map_err(|error| format!("the Slack client: {error}"))?;
        Ok(SlackApp {
            client_id: slack.client_id.expose().to_owned(),
            client_secret: slack.client_secret.expose().to_owned(),
            redirect_uri: redirect_uri(config)?,
            authorize_url: authorize_url.to_owned(),
            access_url: access_url.to_owned(),
            http,
            pending: Mutex::new(HashMap::new()),
            per_ip: config.subscribers.as_ref().map(|subscribers| {
                limit::Window::new(subscribers.signups_per_ip, subscribers.rate_window.get())
            }),
        })
    }

    /// Counts a visitor's sign-in from `ip` against `[subscribers]`
    /// `signups_per_ip`.
    ///
    /// # Errors
    ///
    /// How long to wait, past the limit.
    pub fn admit(&self, ip: IpAddr) -> Result<(), Duration> {
        match &self.per_ip {
            Some(window) => window.try_acquire(&ip),
            None => Ok(()),
        }
    }

    /// Starts a sign-in connecting `purpose`.
    ///
    /// # Errors
    ///
    /// Too many sign-ins are under way, or the random source failed.
    pub fn begin(&self, purpose: Purpose) -> Result<Started, String> {
        let state = token::random()?;
        let nonce = token::random()?;
        let now = Instant::now();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|_, record| record.expires > now);
            if pending.len() >= MAX_PENDING {
                return Err(
                    "too many Slack sign-ins are under way; try again in a few minutes".into(),
                );
            }
            pending.insert(
                token::digest(&state),
                Pending {
                    purpose,
                    nonce_digest: token::digest(&nonce),
                    expires: now + WITHIN,
                },
            );
        }
        let mut location = url::Url::parse(&self.authorize_url)
            .map_err(|error| format!("the Slack authorize URL: {error}"))?;
        location
            .query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("scope", SCOPE)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("state", &state);
        Ok(Started {
            location: location.into(),
            cookie: format!(
                "{COOKIE}={nonce}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
                WITHIN.as_secs()
            ),
        })
    }

    /// What the sign-in `state` connects, if this browser (`headers`)
    /// started it and it has not expired. The record is gone after this,
    /// whatever it answers, so a state works at most once.
    ///
    /// # Errors
    ///
    /// [`Refused`], saying why.
    pub fn finish(&self, state: &str, headers: &HeaderMap) -> Result<Purpose, Refused> {
        if state.is_empty() || state.len() > MAX_PARAMETER {
            return Err(Refused::Unknown);
        }
        let record = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token::digest(state))
            .ok_or(Refused::Unknown)?;
        if record.expires <= Instant::now() {
            return Err(Refused::Unknown);
        }
        let held = nonce(headers).map(token::digest).unwrap_or_default();
        if !token::same_secret(&held, &record.nonce_digest) {
            return Err(Refused::WrongBrowser);
        }
        Ok(record.purpose)
    }

    /// Exchanges Slack's `code` for the channel it connected.
    ///
    /// # Errors
    ///
    /// Slack could not be reached or refused, in words that never carry
    /// the code or the webhook.
    pub async fn exchange(&self, code: &str) -> Result<Granted, String> {
        if code.is_empty() || code.len() > MAX_PARAMETER {
            return Err("Slack sent no usable code".to_owned());
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", code)
            .append_pair("redirect_uri", &self.redirect_uri)
            .finish();
        let credentials = STANDARD.encode(format!("{}:{}", self.client_id, self.client_secret));
        let response = self
            .http
            .post(&self.access_url)
            .header(header::AUTHORIZATION, format!("Basic {credentials}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|error| format!("cannot reach Slack: {}", error.without_url()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("cannot read Slack's answer: {}", error.without_url()))?;
        // The answer holds the webhook, so it is never logged or repeated:
        // only its `error`, one of Slack's codes, is.
        let answer: Answer = serde_json::from_slice(&bytes)
            .map_err(|_| format!("Slack answered {status} with something other than its JSON"))?;
        if !answer.ok {
            let code = answer.error.unwrap_or_default();
            let code: String = code
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(64)
                .collect();
            return Err(format!("Slack refused the sign-in: {code}"));
        }
        let hook = answer.incoming_webhook.ok_or(
            "Slack's answer has no incoming webhook; does the app ask for `incoming-webhook`?",
        )?;
        // Slack's webhooks are https. Plain http is taken only from a
        // plain-http `access_url`: the fake Slack in tests.
        let plain = self.access_url.starts_with("http://") && hook.url.starts_with("http://");
        if !hook.url.starts_with("https://") && !plain {
            return Err("Slack's answer has no usable webhook URL".to_owned());
        }
        let team = answer.team.unwrap_or_default();
        Ok(Granted {
            webhook_url: WebhookUrl::new(hook.url),
            channel: hook.channel.unwrap_or_else(|| "a channel".to_owned()),
            channel_id: hook.channel_id,
            configuration_url: hook.configuration_url,
            team: team.name.unwrap_or_else(|| "Slack".to_owned()),
            team_id: team.id,
        })
    }
}

#[derive(Deserialize)]
struct Answer {
    ok: bool,
    error: Option<String>,
    incoming_webhook: Option<Hook>,
    team: Option<Team>,
}

#[derive(Deserialize)]
struct Hook {
    url: String,
    channel: Option<String>,
    channel_id: Option<String>,
    configuration_url: Option<String>,
}

#[derive(Default, Deserialize)]
struct Team {
    id: Option<String>,
    name: Option<String>,
}

/// The sign-in nonce in `headers`' cookies, if any.
fn nonce(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then_some(value)
        })
}

/// The `Set-Cookie` value clearing a sign-in's cookie.
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax")
}

/// Where Slack sends people back: `public_url` and [`CALLBACK`].
///
/// # Errors
///
/// `[server] public_url` is missing.
pub fn redirect_uri(config: &Config) -> Result<String, String> {
    let base = config
        .server
        .public_url
        .as_deref()
        .ok_or("[slack] needs [server] public_url")?;
    Ok(format!("{}{CALLBACK}", base.trim_end_matches('/')))
}

/// The app's name: `[slack] app_name`, or the page's title cut to what
/// Slack accepts.
pub fn app_name(config: &Config) -> String {
    match config
        .slack
        .as_ref()
        .and_then(|slack| slack.app_name.clone())
    {
        Some(name) => name,
        None => config.page.title.chars().take(MAX_APP_NAME).collect(),
    }
}

/// The Slack app manifest for `config`: one scope, `incoming-webhook`, and
/// the callback under `public_url` as its only redirect URL.
///
/// # Errors
///
/// `[server] public_url` is missing.
pub fn manifest(config: &Config) -> Result<Value, String> {
    let description: String = format!("Status updates from {}", config.page.title)
        .chars()
        .take(140)
        .collect();
    Ok(json!({
        "display_information": {
            "name": app_name(config),
            "description": description,
        },
        "oauth_config": {
            "redirect_urls": [redirect_uri(config)?],
            "scopes": { "bot": [SCOPE] },
        },
        "settings": {
            "incoming_webhooks": { "incoming_webhooks_enabled": true },
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    }))
}

/// The link that opens Slack's create-app flow with `manifest` filled in.
pub fn create_link(manifest: &Value) -> String {
    let encoded: String =
        url::form_urlencoded::byte_serialize(manifest.to_string().as_bytes()).collect();
    format!("{CREATE_APP_URL}{encoded}")
}

/// `tocsin slack setup`: the link creating the operator's own Slack app,
/// its manifest, and the steps after. Secrets the configuration names need
/// not be set yet: the manifest uses none of them.
pub fn setup(folder: &Path) -> Result<Report, Failure> {
    let config = crate::config::load_with(folder, &|name| {
        Some(std::env::var(name).unwrap_or_else(|_| "unset".to_owned()))
    })?
    .config;
    let public_url = config.server.public_url.as_deref().ok_or_else(|| {
        Failure::new(
            Exit::InvalidInput,
            "set `[server] public_url` first: Slack sends people back to it after they approve",
        )
    })?;
    if !public_url.starts_with("https://") {
        return Err(Failure::new(
            Exit::InvalidInput,
            format!(
                "`[server] public_url` is {public_url}; Slack sends people back only to https addresses"
            ),
        ));
    }
    let manifest = manifest(&config).map_err(|error| Failure::new(Exit::InvalidInput, error))?;
    let link = create_link(&manifest);
    let base = public_url.trim_end_matches('/');
    let text = format!(
        "1. Open this link, pick your workspace, and create the app:\n\n   {link}\n\n\
         2. On the app's Basic Information page, put its Client ID and Client Secret in\n   \
         the environment tocsin runs with, such as TOCSIN_SLACK_CLIENT_ID and\n   \
         TOCSIN_SLACK_CLIENT_SECRET. Never in a file.\n\n\
         3. Add to the configuration, then restart `tocsin serve`:\n\n   \
         [slack]\n   \
         client_id = \"${{ENV:TOCSIN_SLACK_CLIENT_ID}}\"\n   \
         client_secret = \"${{ENV:TOCSIN_SLACK_CLIENT_SECRET}}\"\n\n\
         4. Connect alert channels on {base}/admin/slack. With [subscribers],\n   \
         visitors can add their own on {base}/subscribe.\n\n\
         The app asks only for `{SCOPE}`: one channel per sign-in, chosen on Slack's\n\
         consent screen. Slack sends people back to {redirect}.",
        redirect =
            redirect_uri(&config).map_err(|error| Failure::new(Exit::InvalidInput, error))?,
    );
    Ok(Report {
        text,
        exit: Exit::Ok,
        json: json!({ "create_link": link, "manifest": manifest }),
    })
}

/// `text` safe in Slack's mrkdwn: `&`, `<` and `>` escaped, as Slack asks.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn the_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __Host-tocsin-slack=abc; b=2"),
        );
        assert_eq!(nonce(&headers), Some("abc"));
        headers.insert(header::COOKIE, HeaderValue::from_static("tocsin-slack=abc"));
        assert_eq!(nonce(&headers), None);
    }

    #[test]
    fn mrkdwn_control_characters_are_escaped() {
        assert_eq!(
            escape("<!channel> & <x|y>"),
            "&lt;!channel&gt; &amp; &lt;x|y&gt;"
        );
    }
}
