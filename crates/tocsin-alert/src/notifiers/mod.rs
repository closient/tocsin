// SPDX-License-Identifier: AGPL-3.0-only

//! One [`Notifier`] per destination type, and [`build`], which turns a
//! `[[notifier]]` into one.

pub mod alertmanager;
pub mod discord;
pub mod email;
pub mod ntfy;
pub mod pushover;
pub mod slack;
pub mod telegram;
pub mod webhook;

use std::collections::HashMap;
use std::sync::Arc;

use tocsin_core::config::{self, NotifierSettings};

use crate::notifier::Notifier;
use crate::notifiers::slack::SlackHooks;

/// Every notifier, by id.
pub type Notifiers = HashMap<String, Arc<dyn Notifier>>;

/// The notifier a `[[notifier]]` from a loaded configuration describes. A
/// Slack notifier naming a `connection` looks it up in `slack_hooks`.
///
/// # Errors
///
/// The settings cannot be used, such as an email notifier whose server
/// email settings are missing or name a `noreply` address, or a Slack
/// `connection` without `slack_hooks`.
pub fn build(
    notifier: &config::Notifier,
    server_email: Option<&config::ServerEmail>,
    client: &reqwest::Client,
    slack_hooks: Option<&Arc<dyn SlackHooks>>,
) -> Result<Arc<dyn Notifier>, String> {
    let client = client.clone();
    let built: Arc<dyn Notifier> = match notifier.settings() {
        NotifierSettings::Slack(settings) => match (&settings.webhook_url, &settings.connection) {
            (Some(url), _) => Arc::new(slack::Slack::new(client, url.expose().to_owned())),
            (None, Some(name)) => {
                let hooks = slack_hooks
                    .ok_or("a Slack `connection` needs the database, which is not open")?;
                Arc::new(slack::SlackConnection::new(
                    client,
                    name.as_str().to_owned(),
                    Arc::clone(hooks),
                ))
            }
            (None, None) => {
                return Err("`[notifier.slack]` needs `webhook_url` or `connection`".to_owned());
            }
        },
        NotifierSettings::Discord(settings) => Arc::new(discord::Discord::new(
            client,
            settings.webhook_url.expose().to_owned(),
        )),
        NotifierSettings::Ntfy(settings) => Arc::new(ntfy::Ntfy::new(
            client,
            settings.url.clone(),
            settings
                .token
                .as_ref()
                .map(|token| token.expose().to_owned()),
        )),
        NotifierSettings::Telegram(settings) => Arc::new(telegram::Telegram::new(
            client,
            telegram::API_BASE,
            settings.bot_token.expose(),
            settings.chat_id.clone(),
        )),
        NotifierSettings::Pushover(settings) => Arc::new(pushover::Pushover::new(
            client,
            pushover::API_BASE,
            settings.api_token.expose().to_owned(),
            settings.user_key.expose().to_owned(),
            pushover::Priorities::from(settings),
        )),
        NotifierSettings::Webhook(settings) => Arc::new(webhook::Webhook::new(
            client,
            settings.url.clone(),
            settings.signing_secret.expose().to_owned(),
            settings
                .bearer_token
                .as_ref()
                .map(|token| token.expose().to_owned()),
        )),
        NotifierSettings::Alertmanager(settings) => Arc::new(alertmanager::Alertmanager::new(
            client,
            &settings.url,
            settings
                .bearer_token
                .as_ref()
                .map(|token| token.expose().to_owned()),
        )),
        NotifierSettings::Email(settings) => {
            let missing = || {
                "an email notifier needs `[server.email]` with `from`, `reply_to` and `smtp_url`"
                    .to_owned()
            };
            let server = server_email.ok_or_else(missing)?;
            let (Some(from), Some(reply_to), Some(smtp_url)) =
                (&server.from, &server.reply_to, &server.smtp_url)
            else {
                return Err(missing());
            };
            Arc::new(email::Email::new(
                smtp_url.expose(),
                from,
                reply_to,
                &settings.to,
            )?)
        }
    };
    Ok(built)
}

/// Every notifier in `config`, by id.
///
/// # Errors
///
/// A notifier cannot be built; the message names it.
pub fn build_all(
    config: &config::Config,
    client: &reqwest::Client,
    slack_hooks: Option<&Arc<dyn SlackHooks>>,
) -> Result<Notifiers, String> {
    config
        .notifiers
        .iter()
        .map(|notifier| {
            let id = notifier.id.as_str().to_owned();
            build(notifier, config.server.email.as_ref(), client, slack_hooks)
                .map(|built| (id.clone(), built))
                .map_err(|error| format!("notifier `{id}`: {error}"))
        })
        .collect()
}
