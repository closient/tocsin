// SPDX-License-Identifier: AGPL-3.0-only

//! Email subscribers: sign-up with double opt-in, preferences, and the
//! fan-out of incidents and releases. `docs/subscribers.md` describes it
//! for operators.
//!
//! - **Sign-up** ([`Subscriptions::sign_up`]) is shared by
//!   `POST /api/v1/subscribers` and the form on the status page. It checks
//!   the rate limits and Turnstile, stores the address unconfirmed, and
//!   sends the confirmation straight through the [`Outbox`]. The answer is
//!   the same whether the address is new, waiting or already confirmed,
//!   so it cannot tell anyone who subscribes.
//! - **Announcements** ([`Subscriptions::incident_opened`] and friends) are
//!   queued in the store after the write they announce. A [`worker::Postman`]
//!   expands each into one delivery per matching confirmed subscriber and
//!   sends them. The delivery log is unique per (subscriber, announcement)
//!   and a delivery is marked `sending` before it is handed to SMTP, so a
//!   restart never sends one twice: one interrupted mid-send is given up,
//!   not retried.
//! - **Privacy**: confirmation tokens are stored hashed; manage secrets are
//!   compared in constant time; the log names an address only masked.

pub mod limit;
mod message;
pub mod token;
pub mod turnstile;
pub mod worker;

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::request::Parts;
use futures_util::future::BoxFuture;
use tocsin_alert::{Mailer, SendError};
use tocsin_core::config::{Config, Subscribers};
use tocsin_core::incident::{FieldError, Incident};
use tocsin_core::release::Release;
use tocsin_store::{EVERY, NewAnnouncement, NewSignup, Preferences, Scope, Signup, Store};
use tokio::sync::Notify;

use crate::http::{ApiError, AppState};

/// The longest address accepted (RFC 5321's path limit, less the brackets).
const MAX_EMAIL: usize = 254;

/// The pages a subscriber's links lead to.
pub mod paths {
    /// The page that asks to sign up.
    pub const SUBSCRIBE: &str = "/subscribe";

    /// The confirmation link for `token`.
    pub fn confirm(token: &str) -> String {
        format!("{SUBSCRIBE}/confirm/{token}")
    }

    /// The preferences page for manage token `manage`.
    pub fn manage(manage: &str) -> String {
        format!("{SUBSCRIBE}/manage/{manage}")
    }

    /// The one-click unsubscribe link for manage token `manage`.
    pub fn unsubscribe(manage: &str) -> String {
        format!("{SUBSCRIBE}/unsubscribe/{manage}")
    }

    /// Where the form's "Add to Slack" posts.
    pub const SLACK: &str = "/subscribe/slack";

    /// The preferences page for a Slack channel's manage token.
    pub fn slack_manage(manage: &str) -> String {
        format!("{SLACK}/manage/{manage}")
    }

    /// The page that removes a Slack channel, for its manage token.
    pub fn slack_unsubscribe(manage: &str) -> String {
        format!("{SLACK}/unsubscribe/{manage}")
    }
}

/// Hands a finished message to the mail server. The [`Mailer`] in
/// production; a capture in tests, which never sends real mail.
pub trait Outbox: Send + Sync {
    /// Sends `message`, already composed, to `to`.
    fn send<'a>(&'a self, to: &'a str, message: &'a str) -> BoxFuture<'a, Result<(), SendError>>;
}

impl Outbox for Mailer {
    fn send<'a>(&'a self, to: &'a str, message: &'a str) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let envelope = self
                .envelope(&[to.to_owned()])
                .map_err(SendError::permanent)?;
            Mailer::send(self, &envelope, message).await
        })
    }
}

/// What a sign-up asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignupRequest {
    /// The address, as typed.
    pub email: String,
    /// Component ids, or `*` for every component.
    pub components: Vec<String>,
    /// Public project ids, or `*` for every public project.
    pub projects: Vec<String>,
    /// The Turnstile widget's answer, when Turnstile is on.
    pub turnstile: Option<String>,
}

/// The running subscriber feature: present only when `[subscribers]` is.
pub struct Subscriptions {
    settings: Subscribers,
    base: String,
    origin: String,
    mailer: Arc<Mailer>,
    outbox: Arc<dyn Outbox>,
    per_ip: limit::Window<IpAddr>,
    per_address: limit::Window<[u8; 32]>,
    wake: Notify,
    turnstile: Option<turnstile::Verifier>,
    /// `[server.email] reply_to`: who Slack messages say to write to.
    contact: String,
    /// Posts to subscribers' Slack channels.
    http: reqwest::Client,
}

impl std::fmt::Debug for Subscriptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriptions")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Subscriptions {
    /// The feature as `config` sets it up; `None` without `[subscribers]`.
    ///
    /// # Errors
    ///
    /// `[server.email]` or `[server] public_url` is unusable. Config
    /// validation refuses both first, so this is a backstop.
    pub fn from_config(config: &Config) -> Result<Option<Self>, String> {
        if config.subscribers.is_none() {
            return Ok(None);
        }
        let email = config
            .server
            .email
            .as_ref()
            .ok_or("[subscribers] needs [server.email]")?;
        let (Some(from), Some(reply_to), Some(smtp_url)) =
            (&email.from, &email.reply_to, &email.smtp_url)
        else {
            return Err("[subscribers] needs from, reply_to and smtp_url in [server.email]".into());
        };
        let mailer = Arc::new(Mailer::new(smtp_url.expose(), from, reply_to)?);
        let outbox: Arc<dyn Outbox> = Arc::clone(&mailer) as Arc<dyn Outbox>;
        Self::assemble(config, mailer, outbox, turnstile::VERIFY_URL).map(Some)
    }

    /// The feature sending through `outbox` and asking `turnstile_url`, for
    /// tests.
    #[cfg(test)]
    pub fn with_outbox(
        config: &Config,
        outbox: Arc<dyn Outbox>,
        turnstile_url: &str,
    ) -> Result<Self, String> {
        let email = config.server.email.as_ref().ok_or("no [server.email]")?;
        let mailer = Arc::new(Mailer::new(
            "smtp://127.0.0.1:25?tls=none",
            email.from.as_deref().ok_or("no from")?,
            email.reply_to.as_deref().ok_or("no reply_to")?,
        )?);
        Self::assemble(config, mailer, outbox, turnstile_url)
    }

    fn assemble(
        config: &Config,
        mailer: Arc<Mailer>,
        outbox: Arc<dyn Outbox>,
        turnstile_url: &str,
    ) -> Result<Self, String> {
        let settings = config.subscribers.clone().ok_or("no [subscribers] table")?;
        let base = config
            .server
            .public_url
            .as_deref()
            .ok_or("[subscribers] needs [server] public_url")?
            .trim_end_matches('/')
            .to_owned();
        let origin = url::Url::parse(&base)
            .map_err(|error| format!("public_url: {error}"))?
            .origin()
            .ascii_serialization();
        let contact = config
            .server
            .email
            .as_ref()
            .and_then(|email| email.reply_to.clone())
            .ok_or("[subscribers] needs reply_to in [server.email]")?;
        let http = crate::net::http_client(tocsin_alert::HTTP_TIMEOUT)
            .build()
            .map_err(|error| format!("the Slack client: {error}"))?;
        let turnstile = match &settings.turnstile {
            Some(widget) => Some(turnstile::Verifier::new(
                widget.site_key.clone(),
                widget.secret_key.expose().to_owned(),
                turnstile_url.to_owned(),
                crate::net::http_client(tocsin_alert::HTTP_TIMEOUT)
                    .build()
                    .map_err(|error| format!("the Turnstile client: {error}"))?,
            )),
            None => None,
        };
        Ok(Subscriptions {
            per_ip: limit::Window::new(settings.signups_per_ip, settings.rate_window.get()),
            per_address: limit::Window::new(settings.signups_per_email, settings.rate_window.get()),
            settings,
            base,
            origin,
            mailer,
            outbox,
            wake: Notify::new(),
            turnstile,
            contact,
            http,
        })
    }

    /// `[server] public_url`, without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The public URL's origin, which every form post must come from.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The Turnstile site key, when Turnstile is on.
    pub fn turnstile_site_key(&self) -> Option<&str> {
        self.turnstile
            .as_ref()
            .map(|verifier| verifier.site_key.as_str())
    }

    /// The client's address: the TCP peer, or `client_ip_header` when the
    /// peer is one of `trusted_proxies`.
    ///
    /// # Errors
    ///
    /// 500 when the server was started without peer addresses.
    pub fn client_ip(&self, parts: &Parts) -> Result<IpAddr, ApiError> {
        let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() else {
            tracing::error!("the server was started without peer addresses; refusing sign-up");
            return Err(ApiError::internal(&"no peer address"));
        };
        let peer = peer.ip();
        let Some(name) = &self.settings.client_ip_header else {
            return Ok(peer);
        };
        if !self
            .settings
            .trusted_proxies
            .iter()
            .any(|network| network.contains(peer))
        {
            return Ok(peer);
        }
        Ok(parts
            .headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(peer))
    }

    /// Signs `request` up from `ip`. Resolves the same way for a new
    /// address, one waiting for confirmation and one already confirmed;
    /// the mail it sends differs, and goes out after it resolves.
    ///
    /// # Errors
    ///
    /// 422 for an invalid address, unknown components or projects, or a
    /// failed Turnstile check; 429 with a wait past a rate limit; 503 when
    /// Cloudflare cannot be asked; 500 if the store fails.
    pub async fn sign_up(
        &self,
        state: &AppState,
        request: SignupRequest,
        ip: IpAddr,
    ) -> Result<(), ApiError> {
        self.per_ip.try_acquire(&ip).map_err(|wait| {
            tracing::info!(%ip, "sign-up refused: too many from this address");
            ApiError::too_many_requests(
                "too many sign-ups from your address; try again later",
                wait,
            )
        })?;
        let (email, preferences) = validate(state, &request)?;
        if let Some(verifier) = &self.turnstile {
            match verifier.verify(request.turnstile.as_deref(), ip).await {
                turnstile::Verdict::Passed => {}
                turnstile::Verdict::Failed => {
                    return Err(ApiError::invalid(vec![FieldError::new(
                        "turnstile",
                        "the challenge was not passed; try again",
                    )]));
                }
                turnstile::Verdict::Unreachable(cause) => {
                    tracing::warn!(%cause, "Turnstile could not be checked");
                    return Err(ApiError::unavailable(
                        "the challenge could not be checked; try again shortly",
                    ));
                }
            }
        }
        self.per_address
            .try_acquire(&token::address_key(&email))
            .map_err(|wait| {
                tracing::info!(email = %token::mask(&email), "sign-up refused: too many for this address");
                ApiError::too_many_requests("too many sign-ups for this address; try again later", wait)
            })?;

        let confirm = token::random().map_err(|error| ApiError::internal(&error))?;
        let manage_secret = token::random().map_err(|error| ApiError::internal(&error))?;
        let now_ms = tocsin_store::now_ms();
        let within = self.settings.confirm_within.get();
        let signup = state
            .store
            .sign_up(NewSignup {
                email: email.clone(),
                confirm_hash: token::digest(&confirm),
                confirm_expires_at_ms: now_ms
                    .saturating_add(i64::try_from(within.as_millis()).unwrap_or(i64::MAX)),
                manage_secret,
                preferences,
                now_ms,
            })
            .await?;
        let (kind, text) = match &signup {
            Signup::New(_) | Signup::Unconfirmed(_) => (
                "confirm",
                message::confirmation(&state.config, &self.base, &confirm, within),
            ),
            Signup::Confirmed(subscriber) => (
                "already",
                message::already_subscribed(
                    &state.config,
                    &self.base,
                    &token::manage(subscriber.id, &subscriber.manage_secret),
                ),
            ),
        };
        tracing::info!(email = %token::mask(&email), kind, "sign-up accepted");
        let composed = self.compose(
            &email,
            &text,
            &format!("{kind}.{}", token::random().unwrap_or_default()),
            Vec::new(),
            now_ms,
        );
        let outbox = Arc::clone(&self.outbox);
        // Sent after answering, so the answer takes as long either way.
        tokio::spawn(async move {
            let Ok(message) = composed else {
                return;
            };
            if let Err(error) = outbox.send(&email, &message).await {
                tracing::warn!(email = %token::mask(&email), kind, error = %error.message, "sign-up email not sent");
            }
        });
        Ok(())
    }

    /// The whole message `text` for `to`, with `headers` after the
    /// Message-ID.
    ///
    /// # Errors
    ///
    /// `to` cannot be written as a mailbox.
    pub(crate) fn compose(
        &self,
        to: &str,
        text: &message::Text,
        id: &str,
        headers: Vec<(&'static str, String)>,
        now_ms: i64,
    ) -> Result<String, String> {
        Ok(self.mailer.compose(&tocsin_alert::Outgoing {
            to: tocsin_alert::mail::mailbox(to)?,
            subject: text.subject.clone(),
            sent_at_ms: now_ms,
            message_id: format!(
                "<subscriber.{}@{}>",
                tocsin_alert::mail::token(id),
                self.mailer.domain()
            ),
            headers,
            body: text.body.clone(),
        }))
    }

    /// Queues an announcement of the new `incident`.
    pub async fn incident_opened(&self, state: &AppState, incident: &Incident) {
        let text = message::incident_opened(&state.config, &self.base, incident);
        self.announce(
            &state.store,
            format!("incident:{}", incident.id),
            Scope::Component,
            incident.components.clone(),
            text,
        )
        .await;
    }

    /// Queues an announcement of `incident`'s newest update.
    pub async fn incident_updated(&self, state: &AppState, incident: &Incident) {
        let Some(update) = incident.updates.iter().max_by_key(|update| update.id) else {
            return;
        };
        let text = message::incident_updated(&state.config, &self.base, incident, update);
        self.announce(
            &state.store,
            format!("incident:{}:update:{}", incident.id, update.id),
            Scope::Component,
            incident.components.clone(),
            text,
        )
        .await;
    }

    /// Queues an announcement of `release`, if its project is public. A
    /// version is announced once, whichever environment it reaches first.
    pub async fn release_published(&self, state: &AppState, release: &Release) {
        if !state.project_is_public(&release.project) {
            return;
        }
        let text = message::release(&state.config, &self.base, release);
        self.announce(
            &state.store,
            format!("release:{}:{}", release.project, release.version),
            Scope::Project,
            vec![release.project.clone()],
            text,
        )
        .await;
    }

    /// Queues an announcement and wakes the postman. The write it
    /// announces is already stored, so a failure here is logged, not
    /// returned: those subscribers miss this one email.
    async fn announce(
        &self,
        store: &Store,
        key: String,
        scope: Scope,
        targets: Vec<String>,
        text: message::Text,
    ) {
        let announcement = NewAnnouncement {
            key: key.clone(),
            scope,
            targets,
            subject: text.subject,
            body: text.body,
            created_at_ms: tocsin_store::now_ms(),
        };
        match store.queue_announcement(announcement).await {
            Ok(true) => {
                tracing::info!(%key, "announcement queued for subscribers");
                self.wake.notify_one();
            }
            Ok(false) => tracing::debug!(%key, "announcement already queued"),
            Err(error) => {
                tracing::error!(%key, %error, "announcement not queued; subscribers miss it");
            }
        }
    }

    /// Wakes the postman, as a new announcement does.
    pub(crate) fn wake(&self) -> &Notify {
        &self.wake
    }

    pub(crate) fn outbox(&self) -> &dyn Outbox {
        self.outbox.as_ref()
    }

    /// Posts `text` to subscriber channel `connection_id` at its secret
    /// `webhook_url`, with the links that manage it. A webhook Slack says
    /// is revoked marks the connection dead, so nothing is posted to it
    /// again.
    ///
    /// # Errors
    ///
    /// The post failed, and whether it was for good.
    pub(crate) async fn post_slack(
        &self,
        store: &Store,
        connection_id: i64,
        webhook_url: &str,
        manage: &str,
        text: &message::Text,
    ) -> Result<(), SendError> {
        let payload = message::slack(&self.base, manage, &self.contact, text);
        match tocsin_alert::notifiers::slack::post(&self.http, webhook_url, &payload).await {
            Ok(()) => Ok(()),
            Err(Err(error)) => Err(error),
            Err(Ok(reason)) => {
                tracing::warn!(
                    connection = connection_id,
                    %reason,
                    "Slack revoked a subscriber's channel; marking it dead"
                );
                if let Err(error) = store
                    .kill_slack(connection_id, reason.clone(), tocsin_store::now_ms())
                    .await
                {
                    tracing::error!(connection = connection_id, %error, "cannot mark the Slack channel dead");
                }
                Err(SendError::permanent(format!(
                    "Slack revoked the channel ({reason})"
                )))
            }
        }
    }

    /// Welcomes a channel just added: what it will hear about, and how to
    /// change that or remove it. Sent in the background; a failure is
    /// logged, and a revoked webhook marks the channel dead at once.
    pub fn welcome_slack(
        self: &Arc<Self>,
        state: &AppState,
        connection_id: i64,
        webhook_url: String,
        manage: String,
        preferences: &Preferences,
    ) {
        let text = message::slack_welcome(&state.config, &self.base, preferences);
        let store = Arc::clone(&state.store);
        let subscriptions = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = subscriptions
                .post_slack(&store, connection_id, &webhook_url, &manage, &text)
                .await
            {
                tracing::warn!(connection = connection_id, %error, "Slack welcome not posted");
            }
        });
    }
}

/// The normalized address and preferences `request` asks for.
fn validate(state: &AppState, request: &SignupRequest) -> Result<(String, Preferences), ApiError> {
    let email = normalize(&request.email);
    let mut errors = Vec::new();
    if email.is_none() {
        errors.push(FieldError::new("email", "must be an email address"));
    }
    let preferences = choose(state, &request.components, &request.projects, &mut errors);
    match email {
        Some(email) if errors.is_empty() => Ok((email, preferences)),
        _ => Err(ApiError::invalid(errors)),
    }
}

/// The preferences `components` and `projects` ask for, as the manage page
/// submits them.
///
/// # Errors
///
/// 422 naming each unknown component or project, or neither chosen.
pub fn preferences(
    state: &AppState,
    components: &[String],
    projects: &[String],
) -> Result<Preferences, ApiError> {
    let mut errors = Vec::new();
    let preferences = choose(state, components, projects, &mut errors);
    if errors.is_empty() {
        Ok(preferences)
    } else {
        Err(ApiError::invalid(errors))
    }
}

/// Known, deduplicated preferences; each problem is pushed onto `errors`.
fn choose(
    state: &AppState,
    components: &[String],
    projects: &[String],
    errors: &mut Vec<FieldError>,
) -> Preferences {
    let before = errors.len();
    let mut chosen_components = Vec::new();
    for (index, id) in components.iter().enumerate() {
        let id = id.trim();
        if id != EVERY && !state.component_is_public(id) {
            errors.push(FieldError::new(
                format!("components[{index}]"),
                format!("no public component `{id}`"),
            ));
        } else if !chosen_components.iter().any(|known| known == id) {
            chosen_components.push(id.to_owned());
        }
    }
    let mut chosen_projects = Vec::new();
    for (index, id) in projects.iter().enumerate() {
        let id = id.trim();
        if id != EVERY && !state.project_is_public(id) {
            errors.push(FieldError::new(
                format!("projects[{index}]"),
                format!("no public project `{id}`"),
            ));
        } else if !chosen_projects.iter().any(|known| known == id) {
            chosen_projects.push(id.to_owned());
        }
    }
    if errors.len() == before && chosen_components.is_empty() && chosen_projects.is_empty() {
        errors.push(FieldError::new(
            "components",
            "choose at least one component or project",
        ));
    }
    Preferences {
        components: collapse(chosen_components),
        projects: collapse(chosen_projects),
    }
}

/// `*` alone when it is among `ids`.
fn collapse(ids: Vec<String>) -> Vec<String> {
    if ids.iter().any(|id| id == EVERY) {
        vec![EVERY.to_owned()]
    } else {
        ids
    }
}

/// `email` trimmed and lower-cased, if it is a plain address mail can be
/// sent to.
fn normalize(email: &str) -> Option<String> {
    let email = email.trim().to_ascii_lowercase();
    let plain = !email.is_empty()
        && email.len() <= MAX_EMAIL
        && email
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !b"<>()[]\\,;:\"".contains(&byte))
        && tocsin_core::config::email_address(&email).is_ok_and(|address| address == email)
        && tocsin_alert::mail::mailbox(&email).is_ok();
    plain.then_some(email)
}
