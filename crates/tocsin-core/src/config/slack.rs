// SPDX-License-Identifier: AGPL-3.0-only

//! `[slack]`: connecting Slack channels by signing in to Slack.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::Secret;

/// The longest name Slack accepts for an app.
pub const MAX_APP_NAME: usize = 35;

/// `[slack]`: your own Slack app, which people approve on Slack's consent
/// screen to connect a channel. Each connection is an incoming webhook,
/// kept in the database rather than in configuration.
///
/// With it, the admin page can connect channels for `[[notifier]]`s to
/// name with `connection`, and, with `[subscribers]`, visitors can add
/// status updates to their own Slack. `tocsin slack setup` prints the link
/// that creates the app. It needs an `https` `[server] public_url`, which
/// Slack sends people back to. Like `[server]`, it is read only at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    /// The app's Client ID, from its Basic Information page. Secret.
    pub client_id: Secret,
    /// The app's Client Secret, from the same page. Secret.
    pub client_secret: Secret,
    /// The app's name, as Slack shows it on the consent screen and beside
    /// each message, at most 35 characters. Used only by `tocsin slack
    /// setup`; defaults to the page's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
}
