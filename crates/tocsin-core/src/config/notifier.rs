// SPDX-License-Identifier: AGPL-3.0-only

//! `[[notifier]]`: where operator alerts go.
//!
//! A notifier names its `type` and holds that type's settings in a table of
//! the same name, as checks do: `type = "slack"` with `[notifier.slack]`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::{Id, Secret};

/// A destination for operator alerts. Every alert it sends is followed by
/// a resolve with the same identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notifier {
    /// Unique among notifiers in every file.
    pub id: Id,
    /// Display name; the id is used when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Which service the notifier posts to; its settings go in the table of
    /// the same name.
    #[serde(rename = "type")]
    pub notifier_type: NotifierType,
    /// `[notifier.slack]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackSettings>,
    /// `[notifier.pushover]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover: Option<PushoverSettings>,
    /// `[notifier.email]`. Sender addresses come from `[server.email]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<EmailSettings>,
    /// `[notifier.ntfy]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntfy: Option<NtfySettings>,
    /// `[notifier.discord]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordSettings>,
    /// `[notifier.telegram]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramSettings>,
    /// `[notifier.webhook]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookSettings>,
    /// `[notifier.alertmanager]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alertmanager: Option<AlertmanagerSettings>,
}

/// The service a notifier posts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotifierType {
    /// A Slack incoming webhook.
    Slack,
    /// Pushover.
    Pushover,
    /// Email over SMTP.
    Email,
    /// An ntfy topic.
    Ntfy,
    /// A Discord webhook.
    Discord,
    /// A Telegram bot.
    Telegram,
    /// A generic JSON webhook.
    Webhook,
    /// The Alertmanager v2 alerts API.
    Alertmanager,
}

impl NotifierType {
    /// Every notifier type, in declaration order.
    pub const ALL: [NotifierType; 8] = [
        NotifierType::Slack,
        NotifierType::Pushover,
        NotifierType::Email,
        NotifierType::Ntfy,
        NotifierType::Discord,
        NotifierType::Telegram,
        NotifierType::Webhook,
        NotifierType::Alertmanager,
    ];

    /// The name used in configuration, which is also its settings table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NotifierType::Slack => "slack",
            NotifierType::Pushover => "pushover",
            NotifierType::Email => "email",
            NotifierType::Ntfy => "ntfy",
            NotifierType::Discord => "discord",
            NotifierType::Telegram => "telegram",
            NotifierType::Webhook => "webhook",
            NotifierType::Alertmanager => "alertmanager",
        }
    }
}

/// A notifier's settings, borrowed from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotifierSettings<'a> {
    /// See [`SlackSettings`].
    Slack(&'a SlackSettings),
    /// See [`PushoverSettings`].
    Pushover(&'a PushoverSettings),
    /// See [`EmailSettings`].
    Email(&'a EmailSettings),
    /// See [`NtfySettings`].
    Ntfy(&'a NtfySettings),
    /// See [`DiscordSettings`].
    Discord(&'a DiscordSettings),
    /// See [`TelegramSettings`].
    Telegram(&'a TelegramSettings),
    /// See [`WebhookSettings`].
    Webhook(&'a WebhookSettings),
    /// See [`AlertmanagerSettings`].
    Alertmanager(&'a AlertmanagerSettings),
}

impl Notifier {
    /// The types whose settings tables are present.
    #[must_use]
    pub fn settings_present(&self) -> Vec<NotifierType> {
        NotifierType::ALL
            .into_iter()
            .filter(|notifier_type| self.settings_for(*notifier_type).is_some())
            .collect()
    }

    fn settings_for(&self, notifier_type: NotifierType) -> Option<NotifierSettings<'_>> {
        match notifier_type {
            NotifierType::Slack => self.slack.as_ref().map(NotifierSettings::Slack),
            NotifierType::Pushover => self.pushover.as_ref().map(NotifierSettings::Pushover),
            NotifierType::Email => self.email.as_ref().map(NotifierSettings::Email),
            NotifierType::Ntfy => self.ntfy.as_ref().map(NotifierSettings::Ntfy),
            NotifierType::Discord => self.discord.as_ref().map(NotifierSettings::Discord),
            NotifierType::Telegram => self.telegram.as_ref().map(NotifierSettings::Telegram),
            NotifierType::Webhook => self.webhook.as_ref().map(NotifierSettings::Webhook),
            NotifierType::Alertmanager => self
                .alertmanager
                .as_ref()
                .map(NotifierSettings::Alertmanager),
        }
    }

    /// The settings for the notifier's type.
    ///
    /// # Panics
    ///
    /// Panics if the settings table is missing, which it never is in a
    /// loaded configuration.
    #[must_use]
    pub fn settings(&self) -> NotifierSettings<'_> {
        self.settings_for(self.notifier_type)
            .expect("config::load ensures each notifier has its settings table")
    }

    /// Every secret the notifier holds, with its path below the notifier.
    pub(crate) fn secrets_mut(&mut self) -> Vec<(Vec<&'static str>, &mut Secret)> {
        let mut secrets = Vec::new();
        if let Some(slack) = &mut self.slack {
            secrets.push((vec!["slack", "webhook_url"], &mut slack.webhook_url));
        }
        if let Some(pushover) = &mut self.pushover {
            secrets.push((vec!["pushover", "user_key"], &mut pushover.user_key));
            secrets.push((vec!["pushover", "api_token"], &mut pushover.api_token));
        }
        if let Some(ntfy) = &mut self.ntfy
            && let Some(token) = &mut ntfy.token
        {
            secrets.push((vec!["ntfy", "token"], token));
        }
        if let Some(discord) = &mut self.discord {
            secrets.push((vec!["discord", "webhook_url"], &mut discord.webhook_url));
        }
        if let Some(telegram) = &mut self.telegram {
            secrets.push((vec!["telegram", "bot_token"], &mut telegram.bot_token));
        }
        if let Some(webhook) = &mut self.webhook
            && let Some(token) = &mut webhook.bearer_token
        {
            secrets.push((vec!["webhook", "bearer_token"], token));
        }
        if let Some(alertmanager) = &mut self.alertmanager
            && let Some(token) = &mut alertmanager.bearer_token
        {
            secrets.push((vec!["alertmanager", "bearer_token"], token));
        }
        secrets
    }
}

/// `[notifier.slack]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlackSettings {
    /// The incoming webhook URL. Secret.
    pub webhook_url: Secret,
}

/// `[notifier.pushover]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushoverSettings {
    /// The user or group key alerts go to. Secret.
    pub user_key: Secret,
    /// The application's API token. Secret.
    pub api_token: Secret,
}

/// `[notifier.email]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailSettings {
    /// Recipient addresses.
    pub to: Vec<String>,
}

/// `[notifier.ntfy]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NtfySettings {
    /// The topic URL, such as `https://ntfy.sh/my-alerts`.
    pub url: String,
    /// An access token, for a protected topic. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Secret>,
}

/// `[notifier.discord]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscordSettings {
    /// The channel webhook URL. Secret.
    pub webhook_url: Secret,
}

/// `[notifier.telegram]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelegramSettings {
    /// The bot's token. Secret.
    pub bot_token: Secret,
    /// The chat alerts are sent to.
    pub chat_id: String,
}

/// `[notifier.webhook]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookSettings {
    /// The URL alerts are posted to as JSON.
    pub url: String,
    /// Sent as `Authorization: Bearer ...`. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<Secret>,
}

/// `[notifier.alertmanager]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AlertmanagerSettings {
    /// The Alertmanager base URL, such as `http://alertmanager:9093`.
    pub url: String,
    /// Sent as `Authorization: Bearer ...`. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<Secret>,
}
