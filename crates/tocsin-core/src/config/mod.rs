// SPDX-License-Identifier: AGPL-3.0-only

//! The configuration model: what a folder of TOML files describes.
//!
//! Every `*.toml` file in the folder is a [`ConfigFile`]; [`load`] parses
//! them, merges them into one [`Config`] and validates it, reporting every
//! problem as a [`Diagnostic`] with its file, line and column. The field
//! reference is `docs/config.md`; the JSON Schema comes from [`schema`].

mod assertion;
mod check;
mod diagnostic;
mod load;
mod notifier;
mod values;

use std::net::SocketAddr;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use assertion::{
    Assertion, CONDITION_KEYS, Comparison, Condition, FailureState, HeaderAssertion, JsonAssertion,
    RecordsAssertion, TextAssertion,
};
pub use check::{
    BasicAuth, Check, CheckType, DnsSettings, HttpMethod, HttpSettings, RecordType, TcpSettings,
    TlsSettings,
};
pub use diagnostic::Diagnostic;
pub use load::{Environment, Source, closest, host_and_port, load};
pub use notifier::{
    AlertmanagerSettings, DiscordSettings, EmailSettings, Notifier, NotifierSettings, NotifierType,
    NtfySettings, PushoverSettings, SlackSettings, TelegramSettings, WebhookSettings,
};
pub use values::{
    Cidr, DURATION_PATTERN, Duration, ID_PATTERN, Id, SECRET_PATTERN, Secret, Text, email_address,
    is_unanswerable,
};

/// One configuration file. Every key is optional in a single file; rules
/// that span files (unique ids, references, required settings) are checked
/// after merging.
///
/// This is the type the JSON Schema describes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "tocsin configuration file")]
pub struct ConfigFile {
    /// Server settings. At most one file may hold `[server]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    /// What the status page shows, each with a health state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub component: Vec<Component>,
    /// Headings that collect components on the status page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group: Vec<Group>,
    /// Probes run on an interval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check: Vec<Check>,
    /// Pings tocsin expects to receive on an interval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heartbeat: Vec<Heartbeat>,
    /// Projects whose releases and deploys the release log records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project: Vec<Project>,
    /// Destinations for operator alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifier: Vec<Notifier>,
    /// Which alerts go to which notifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route: Vec<Route>,
    /// Bearer tokens for the write API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token: Vec<Token>,
    /// The public status page. At most one file may hold `[page]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
}

/// The merged, validated configuration of a whole folder.
///
/// Field names match the TOML keys, so `tocsin config show` reads like the
/// files it came from.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    /// `[server]`, with defaults filled in.
    pub server: Server,
    /// Every `[[component]]`, in file order.
    #[serde(rename = "component")]
    pub components: Vec<Component>,
    /// Every `[[group]]`, in file order.
    #[serde(rename = "group")]
    pub groups: Vec<Group>,
    /// Every `[[check]]`, in file order.
    #[serde(rename = "check")]
    pub checks: Vec<Check>,
    /// Every `[[heartbeat]]`, in file order.
    #[serde(rename = "heartbeat")]
    pub heartbeats: Vec<Heartbeat>,
    /// Every `[[project]]`, in file order.
    #[serde(rename = "project")]
    pub projects: Vec<Project>,
    /// Every `[[notifier]]`, in file order.
    #[serde(rename = "notifier")]
    pub notifiers: Vec<Notifier>,
    /// Every `[[route]]`, in file order.
    #[serde(rename = "route")]
    pub routes: Vec<Route>,
    /// Every `[[token]]`, in file order.
    #[serde(rename = "token")]
    pub tokens: Vec<Token>,
    /// `[page]`, with defaults filled in.
    pub page: Page,
}

/// `[server]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The address the web server listens on, as `ip:port`: `127.0.0.1:8080`,
    /// `0.0.0.0:8080` or `[::]:8080`. Hostnames are not accepted.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The URL people reach the status page at, used in links and emails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// This node's name in every result it records. Defaults to the
    /// hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The folder holding the SQLite database. `tocsin serve --data`
    /// overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    /// Days of check results kept.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// The most checks that may run at once across the node. Checks due
    /// while the limit is reached wait for a free slot.
    #[serde(default = "default_max_concurrent_checks")]
    #[schemars(range(min = 1))]
    pub max_concurrent_checks: u32,
    /// Sender settings for every email tocsin sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<ServerEmail>,
    /// How the admin page authenticates people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: default_listen(),
            public_url: None,
            node_id: None,
            data_dir: None,
            retention_days: default_retention_days(),
            max_concurrent_checks: default_max_concurrent_checks(),
            email: None,
            auth: None,
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_retention_days() -> u32 {
    90
}

fn default_max_concurrent_checks() -> u32 {
    64
}

/// `[server.email]`. Required, with `from`, `reply_to` and `smtp_url`,
/// whenever an email notifier exists.
///
/// Every email tocsin sends can be answered: neither address may be a
/// `noreply`, `no-reply` or `donotreply` address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerEmail {
    /// The From address, as `address` or `Name <address>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The Reply-To address: a mailbox a person reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The SMTP server, as `smtp://user:password@host:587` or
    /// `smtps://...`. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_url: Option<Secret>,
}

/// `[server.auth]`: how the admin page at `/admin/` learns who is asking.
///
/// tocsin never handles passwords. It sits behind an identity-aware proxy
/// and believes only what that proxy can prove: a signed Cloudflare Access
/// token, or a header from a peer address you trust. Without this table,
/// or with `mode = "none"`, the admin page does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// How identities are proven. `none` turns the admin page off.
    #[serde(default)]
    pub mode: AuthMode,
    /// `cloudflare_access` only: the Zero Trust team, as `name` or
    /// `name.cloudflareaccess.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_domain: Option<String>,
    /// `cloudflare_access` only: the Access application's audience (AUD)
    /// tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// `trusted_header` only: the request header carrying the email
    /// address. Defaults to `X-Forwarded-Email`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// `trusted_header` only: the networks the proxy connects from. The
    /// header is believed only when the TCP peer is inside one of them;
    /// `X-Forwarded-For` is never consulted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<Cidr>,
    /// Email addresses allowed in. With neither this nor
    /// `allowed_domains`, everyone the proxy lets through is allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_emails: Vec<String>,
    /// Email domains allowed in, matched exactly (`example.com` does not
    /// admit `team.example.com`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

/// The default header for [`AuthMode::TrustedHeader`].
pub const DEFAULT_IDENTITY_HEADER: &str = "X-Forwarded-Email";

/// How [`Auth`] proves who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// No admin page.
    #[default]
    None,
    /// Cloudflare Access: verify the signed `Cf-Access-Jwt-Assertion`
    /// token against the team's published keys.
    CloudflareAccess,
    /// A proxy on a trusted network names the person in a header.
    TrustedHeader,
}

/// `[page]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Page {
    /// The page title.
    #[serde(default = "default_title")]
    pub title: String,
    /// A line shown under the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the page's logo and title link to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage_url: Option<String>,
}

impl Default for Page {
    fn default() -> Self {
        Page {
            title: default_title(),
            description: None,
            homepage_url: None,
        }
    }
}

fn default_title() -> String {
    "Status".to_owned()
}

/// `[[component]]`: something the status page shows with a health state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// Unique among components in every file.
    pub id: Id,
    /// Display name.
    pub name: String,
    /// A line shown with the component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The `[[group]]` the component is shown under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Id>,
    /// How the health of the component's checks combines into its own.
    #[serde(default)]
    pub rule: ComponentRule,
}

/// How a component's health is derived from the health of its checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ComponentRule {
    /// The worst check decides: one check down makes the component down.
    #[default]
    Worst,
    /// The worst state that more than half of the checks share or exceed:
    /// one check down out of three leaves the component up.
    Majority,
}

/// `[[group]]`: a heading collecting components on the status page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Unique among groups in every file.
    pub id: Id,
    /// Display name.
    pub name: String,
    /// A line shown under the heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `[[heartbeat]]`: a ping tocsin expects to receive, such as a cron job
/// reporting in. Pings are sent to the write API with a token that has the
/// `heartbeat` scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// Unique among heartbeats in every file.
    pub id: Id,
    /// Display name; the id is shown when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `[[component]]` whose health this heartbeat sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<Id>,
    /// How often a ping is expected.
    pub interval: Duration,
    /// How late a ping may be before the heartbeat is down.
    #[serde(default = "default_grace")]
    pub grace: Duration,
    /// Links shown with the heartbeat, such as the job's logs or runbook,
    /// each a `[[heartbeat.link]]` table.
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

/// A `[[check.link]]` or `[[heartbeat.link]]`: a named link to somewhere an
/// operator looks next, such as a dashboard, logs or a runbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Link {
    /// The link text.
    pub name: String,
    /// Where it goes: an absolute `http://` or `https://` URL.
    pub url: String,
}

fn default_grace() -> Duration {
    Duration::from_secs(60)
}

/// `[[project]]`: something whose releases and deploys the release log
/// records. CI pushes them; tocsin never polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// Unique among projects in every file.
    pub id: Id,
    /// Display name.
    pub name: String,
    /// A line shown with the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The project's homepage or repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The `[[component]]`s this project's deploys belong to: a timeline
    /// filtered to one of them shows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Id>,
    /// Whether `/releases`, its feed and the unauthenticated API show the
    /// project's releases. A non-public project's releases are seen only
    /// with a `status:read` token or on the admin timeline.
    #[serde(default = "default_public")]
    pub public: bool,
}

fn default_public() -> bool {
    true
}

/// `[[route]]`: sends alerts matching its filters to its notifiers. Empty
/// filters match everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Unique among routes in every file.
    pub id: Id,
    /// The `[[notifier]]`s that receive matching alerts.
    pub notifiers: Vec<Id>,
    /// Only alerts for these components.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Id>,
    /// Only alerts for components in these groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Id>,
    /// Only alerts for these checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Id>,
    /// Only alerts for these heartbeats.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heartbeats: Vec<Id>,
    /// The least severe state that alerts: `degraded` alerts on degraded and
    /// down, `down` on down only.
    #[serde(default)]
    pub notify_on: FailureState,
}

/// `[[token]]`: a bearer token for the write API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Token {
    /// Unique among tokens in every file; logged with each write.
    pub id: Id,
    /// The token's value. Secret.
    pub secret: Secret,
    /// What the token may write.
    pub scopes: Vec<TokenScope>,
    /// Who or what uses the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What a token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    /// Heartbeat pings.
    Heartbeat,
    /// Incidents and maintenance.
    Incident,
    /// Releases and deploys.
    Release,
    /// Reading what is not public: non-public projects' releases in the
    /// read API.
    // TODO(TOC-48): the private status page reads with this scope too.
    #[serde(rename = "status:read")]
    StatusRead,
}

/// The JSON Schema for one configuration file ([`ConfigFile`]), in draft 7
/// for the widest editor support.
#[must_use]
pub fn schema() -> serde_json::Value {
    let schema = schemars::generate::SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<ConfigFile>();
    schema.to_value()
}
