// SPDX-License-Identifier: AGPL-3.0-only

//! Command-line definition.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use tocsin_core::incident::{Impact, IncidentStatus, Timestamp};

pub use crate::http::heartbeats::PingStatus;

/// Single-binary uptime monitor, status page and release log.
#[derive(Debug, Parser)]
#[command(name = "tocsin", disable_version_flag = true)]
pub struct Cli {
    /// Print the version and exit.
    ///
    /// Declared by hand rather than through clap so that `--json` applies.
    /// Not global: `tocsin release` takes a `--version` of its own.
    #[arg(short = 'V', long, action = ArgAction::SetTrue)]
    pub version: bool,

    /// Write exactly one JSON document to stdout instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the monitor until SIGINT or SIGTERM.
    Serve {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,

        /// Folder for the SQLite database; created if missing. Defaults to
        /// `server.data_dir` from the configuration.
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
    },
    /// Validate, show or describe the configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect and maintain the SQLite database.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Run checks by hand.
    Probe {
        #[command(subcommand)]
        command: ProbeCommand,
    },
    /// Open, update, list and export incidents on a running server.
    Incident {
        #[command(subcommand)]
        command: IncidentCommand,
    },
    /// Announce scheduled maintenance on a running server.
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
    /// Publish releases and stamp deploys on a running server.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Hold alert messages back for a while on a running server. Needs a
    /// token with the `silence` scope.
    Silence {
        #[command(subcommand)]
        command: SilenceCommand,
    },
    /// Try the alert notifiers.
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },
    /// Set up the Slack app that connects channels by signing in.
    Slack {
        #[command(subcommand)]
        command: SlackCommand,
    },
    /// Ping a heartbeat on a running server, from the job it watches.
    Heartbeat {
        #[command(subcommand)]
        command: HeartbeatCommand,
    },
    /// Print the OpenAPI document for the HTTP API.
    Openapi,
    /// The status now on a running server: overall, every component and
    /// the open incidents. Exits 1 when anything is degraded or down.
    Status {
        #[command(flatten)]
        server: ServerArgs,
    },
    /// One check's or heartbeat's raw results, newest first. Needs a token
    /// with the `status:read` scope.
    History {
        #[command(flatten)]
        server: ServerArgs,

        /// A `[[check]]` or `[[heartbeat]]` id.
        check: String,

        #[command(flatten)]
        page: HistoryArgs,
    },
    /// Outages open at any time in the window, newest first.
    Outages {
        #[command(flatten)]
        server: ServerArgs,

        #[command(flatten)]
        page: HistoryArgs,
    },
    /// Outages, incidents and deploys, oldest first. Non-public projects'
    /// deploys need a token with the `status:read` scope.
    Timeline {
        #[command(flatten)]
        server: ServerArgs,

        /// Only events touching this `[[component]]`.
        #[arg(long, value_name = "ID")]
        component: Option<String>,

        /// The start of the window: a duration back from now, such as `1d`,
        /// or RFC 3339. At most 366 days back; defaults to 30 days.
        #[arg(long, value_name = "WHEN", value_parser = crate::query::parse_since)]
        since: Option<Timestamp>,

        /// Only events after this: the previous page's `next_after`, with
        /// the same `--since`.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,

        /// Events, 1 to 500; defaults to 100.
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Serve the MCP tools over stdin and stdout for an agent, calling a
    /// running server's HTTP API. Stdout carries only the protocol.
    Mcp {
        #[command(flatten)]
        server: ServerArgs,
    },
}

/// The window and page of a newest-first history read.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// The start of the window: a duration back from now, such as `2h` or
    /// `7d`, or RFC 3339. At most 366 days back; defaults to one day for
    /// results and 30 days for outages.
    #[arg(long, value_name = "WHEN", value_parser = crate::query::parse_since)]
    pub since: Option<Timestamp>,

    /// Only rows older than this: the previous page's `next_before`, with
    /// the same `--since`.
    #[arg(long, value_name = "ID")]
    pub before: Option<i64>,

    /// Rows, 1 to 500; defaults to 100.
    #[arg(long)]
    pub limit: Option<u32>,
}

/// Which server to talk to, and as whom.
#[derive(Debug, Args)]
pub struct ServerArgs {
    /// Base URL of the tocsin server, such as `https://status.example.com`.
    #[arg(long, env = "TOCSIN_URL", value_name = "URL")]
    pub url: String,

    /// A `[[token]]` secret with the scope the command needs: `incident`
    /// (or `incident:draft`, for drafts only) for incidents and
    /// maintenance, `release` for releases, `heartbeat`
    /// for pings, `silence` for silences, `status:read` to read what is
    /// not public and for `config status`. Prefer the environment variable: arguments show in
    /// `ps`.
    #[arg(
        long,
        env = "TOCSIN_TOKEN",
        hide_env_values = true,
        value_name = "TOKEN"
    )]
    pub token: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum IncidentCommand {
    /// Open an incident.
    New {
        #[command(flatten)]
        server: ServerArgs,

        /// One line, shown on the status page.
        #[arg(long)]
        title: String,

        /// none, minor, major or critical. Defaults to none.
        #[arg(long)]
        impact: Option<Impact>,

        /// An affected `[[component]]` id; repeat for several.
        #[arg(long = "component", value_name = "ID")]
        components: Vec<String>,

        /// The first update, in markdown.
        #[arg(long)]
        message: String,

        /// The first status. Defaults to investigating.
        #[arg(long)]
        status: Option<IncidentStatus>,

        /// Open it without emailing subscribers.
        #[arg(long)]
        quiet: bool,

        /// Save it as a draft: seen only by signed-in people and
        /// `status:read` tokens, announced to nobody, until `tocsin
        /// incident publish`. An `incident:draft` token always drafts.
        #[arg(long)]
        draft: bool,

        /// A note for whoever publishes it: evidence, links. Never shown
        /// publicly; read back only with a `status:read` token.
        #[arg(long, value_name = "TEXT")]
        internal_note: Option<String>,

        /// Your own key for what this reports, such as an alert cluster's.
        /// If an open incident already has it, that incident is answered
        /// and nothing is opened; a resolved one has freed it.
        #[arg(long, value_name = "KEY")]
        external_key: Option<String>,
    },
    /// Publish a draft: show it publicly and tell subscribers once. Needs
    /// the `incident` scope.
    Publish {
        #[command(flatten)]
        server: ServerArgs,

        /// The draft's id.
        id: i64,

        /// Publish it without emailing subscribers.
        #[arg(long)]
        quiet: bool,
    },
    /// Delete a draft and its updates. A published incident stays.
    Discard {
        #[command(flatten)]
        server: ServerArgs,

        /// The draft's id.
        id: i64,
    },
    /// Post an update, moving the incident to a status.
    Update {
        #[command(flatten)]
        server: ServerArgs,

        /// The incident's id.
        id: i64,

        /// investigating, identified, monitoring or resolved; for
        /// maintenance, scheduled, `in_progress` or completed.
        #[arg(long)]
        status: IncidentStatus,

        /// The update, in markdown.
        #[arg(long)]
        message: String,

        /// Post it without emailing subscribers.
        #[arg(long)]
        quiet: bool,
    },
    /// Change the title, impact, components or schedule.
    Edit {
        #[command(flatten)]
        server: ServerArgs,

        /// The incident's id.
        id: i64,

        /// A new title.
        #[arg(long)]
        title: Option<String>,

        /// A new impact.
        #[arg(long)]
        impact: Option<Impact>,

        /// The new set of affected components, replacing the old; repeat
        /// for several.
        #[arg(
            long = "component",
            value_name = "ID",
            conflicts_with = "no_components"
        )]
        components: Vec<String>,

        /// Clear the affected components.
        #[arg(long)]
        no_components: bool,

        /// A new planned start (maintenance), in RFC 3339.
        #[arg(long, value_name = "TIME")]
        start: Option<Timestamp>,

        /// A new planned end (maintenance), in RFC 3339.
        #[arg(long, value_name = "TIME")]
        end: Option<Timestamp>,

        /// A new internal note, replacing the old; "" clears it. Never
        /// shown publicly.
        #[arg(long, value_name = "TEXT")]
        internal_note: Option<String>,
    },
    /// Show one incident with every update. A draft needs a token with
    /// the `status:read` scope.
    Show {
        #[command(flatten)]
        server: ServerArgs,

        /// The incident's id.
        id: i64,
    },
    /// List incidents and maintenance, newest first; with a `status:read`
    /// token, drafts too.
    List {
        #[command(flatten)]
        server: ServerArgs,

        /// How many, 1 to 100.
        #[arg(long, default_value_t = 20)]
        limit: u32,

        /// Only incidents with a smaller id: the previous page's
        /// `next_before`.
        #[arg(long, value_name = "ID")]
        before: Option<i64>,

        /// Only incidents with this external key, newest first, resolved
        /// ones included. Needs a `status:read` token.
        #[arg(long, value_name = "KEY")]
        external_key: Option<String>,
    },
    /// Write published incidents as markdown with front matter. Drafts are
    /// skipped by `--all` and refused by id.
    Export {
        #[command(flatten)]
        server: ServerArgs,

        /// The incident's id.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<i64>,

        /// Every incident and maintenance, one file each; needs `--out`.
        #[arg(long, requires = "out")]
        all: bool,

        /// Output format.
        #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
        format: ExportFormat,

        /// Folder to write `incident-<id>.md` files to, created if
        /// missing. Without it, one incident is printed.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MaintenanceCommand {
    /// Announce maintenance ahead of time.
    Schedule {
        #[command(flatten)]
        server: ServerArgs,

        /// One line, shown on the status page.
        #[arg(long, default_value = "Scheduled maintenance")]
        title: String,

        /// When it begins, in RFC 3339 such as `2026-11-01T02:00:00Z`.
        #[arg(long, value_name = "TIME")]
        start: Timestamp,

        /// When it ends, after `--start`.
        #[arg(long, value_name = "TIME")]
        end: Timestamp,

        /// An affected `[[component]]` id; repeat for several.
        #[arg(long = "component", value_name = "ID")]
        components: Vec<String>,

        /// What will happen, in markdown.
        #[arg(long)]
        message: String,

        /// none, minor, major or critical. Defaults to none.
        #[arg(long)]
        impact: Option<Impact>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SilenceCommand {
    /// Hold back the alert messages the matchers name until `--expires`.
    /// Give at least one matcher, or `--episode`. Who made it is the
    /// token; it can be ended early but never extended.
    New {
        #[command(flatten)]
        server: ServerArgs,

        /// Only alerts from this node.
        #[arg(long, value_name = "NODE")]
        node: Option<String>,

        /// Only alerts for checks, or only for heartbeats.
        #[arg(long, value_name = "KIND")]
        subject_kind: Option<SubjectKindArg>,

        /// Only alerts for this `[[check]]` or `[[heartbeat]]`.
        #[arg(long, value_name = "ID")]
        subject: Option<String>,

        /// Only alerts for checks on this `[[component]]`.
        #[arg(long, value_name = "ID")]
        component: Option<String>,

        /// Only messages to this `[[notifier]]`.
        #[arg(long, value_name = "ID")]
        notifier: Option<String>,

        /// Only during this outage: the silence ends when it does, and
        /// matches its node and subject unless told otherwise.
        #[arg(long, value_name = "OUTAGE")]
        episode: Option<i64>,

        /// When it ends: a duration from now, such as `30m` or `4h`, or
        /// RFC 3339. At most `[server] max_silence` away.
        #[arg(long, value_name = "WHEN", value_parser = crate::silence::parse_expires)]
        expires: Timestamp,

        /// Why, for whoever reads the silence later.
        #[arg(long)]
        reason: String,
    },
    /// List silences, newest first, active or over.
    List {
        #[command(flatten)]
        server: ServerArgs,

        /// How many, 1 to 100.
        #[arg(long, default_value_t = 20)]
        limit: u32,

        /// Only silences with a smaller id: the previous page's
        /// `next_before`.
        #[arg(long, value_name = "ID")]
        before: Option<i64>,
    },
    /// One silence, with the messages it held back.
    Show {
        #[command(flatten)]
        server: ServerArgs,

        /// The silence's id.
        id: i64,
    },
    /// End a silence now.
    Expire {
        #[command(flatten)]
        server: ServerArgs,

        /// The silence's id.
        id: i64,
    },
}

/// `--subject-kind`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SubjectKindArg {
    /// A `[[check]]`.
    Check,
    /// A `[[heartbeat]]`.
    Heartbeat,
}

#[derive(Debug, Subcommand)]
pub enum ReleaseCommand {
    /// Publish a release, or update the one with the same project, version
    /// and environment.
    Publish {
        #[command(flatten)]
        server: ServerArgs,

        /// A `[[project]]` id.
        #[arg(long, value_name = "ID")]
        project: String,

        /// The version, such as `v1.4.0`: 1 to 64 ASCII letters, digits,
        /// `.`, `_`, `+` or `-`, starting with a letter or digit.
        #[arg(long, value_name = "VERSION")]
        version: String,

        /// One line, at most 200 characters. Defaults to the version.
        #[arg(long)]
        title: Option<String>,

        /// Release notes in markdown, at most 64 KiB; `-` reads stdin.
        #[arg(long, value_name = "FILE")]
        notes_file: Option<PathBuf>,

        /// Defaults to `production`.
        #[arg(long, value_name = "NAME")]
        environment: Option<String>,

        /// An http or https link to the release, such as its GitHub
        /// release page. (`--url` is the tocsin server.)
        #[arg(long, value_name = "URL")]
        release_url: Option<String>,

        /// When it was released: `now` or RFC 3339. Defaults to now on the
        /// first publish and to the recorded time after.
        #[arg(long, value_name = "TIME", value_parser = crate::release::parse_when)]
        released_at: Option<Timestamp>,

        /// When it was deployed, if it already has been: `now` or RFC 3339.
        #[arg(long, value_name = "TIME", value_parser = crate::release::parse_when)]
        deployed_at: Option<Timestamp>,
    },
    /// Record that a version was deployed. A version not yet published is
    /// recorded as a stub its publish fills in.
    Deployed {
        #[command(flatten)]
        server: ServerArgs,

        /// A `[[project]]` id.
        #[arg(long, value_name = "ID")]
        project: String,

        /// The version, as published.
        #[arg(long, value_name = "VERSION")]
        version: String,

        /// Defaults to `production`.
        #[arg(long, value_name = "NAME")]
        environment: Option<String>,

        /// `now` (the default) or RFC 3339.
        #[arg(long, value_name = "TIME", value_parser = crate::release::parse_when)]
        deployed_at: Option<Timestamp>,
    },
}

#[derive(Debug, Subcommand)]
pub enum HeartbeatCommand {
    /// Record that the job ran. Exits 0 once the server has recorded the
    /// ping, whichever status it carries.
    Ping {
        #[command(flatten)]
        server: ServerArgs,

        /// The heartbeat's id.
        #[arg(value_name = "HEARTBEAT_ID")]
        id: String,

        /// Whether the job succeeded.
        #[arg(long, value_enum, default_value_t = PingStatus::Up)]
        status: PingStatus,

        /// A short note stored with the result, such as what failed.
        #[arg(long, help = concat!(
            "A short note stored with the result, such as what failed; at most ",
            tocsin_engine::ping_message_limit!(),
            " bytes",
        ))]
        message: Option<String>,
    },
}

/// Formats `incident export` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// Markdown with YAML front matter.
    Md,
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Report the database size, schema version and row counts.
    ///
    /// Read-only: never migrates, so it also describes a database written by
    /// a newer tocsin.
    Info {
        /// Folder holding `tocsin.db`.
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
    },

    /// Rebuild the database file to reclaim space freed by retention.
    ///
    /// Refused while `tocsin serve` is running on the same folder.
    Vacuum {
        /// Folder holding `tocsin.db`.
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
    },

    /// Recompute daily rollups from raw results, from a UTC day onwards.
    ///
    /// Days already finalized by retention are left as they are.
    Rollup {
        /// Folder holding `tocsin.db`.
        #[arg(long, value_name = "DIR")]
        data: PathBuf,

        /// First UTC day to recompute, as YYYY-MM-DD.
        #[arg(long, value_name = "DATE")]
        since: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProbeCommand {
    /// Run one check once from the configuration, with no server and no
    /// database, and print the facts the probe gathered and each
    /// assertion's verdict. Exits 0 when the check is up, 1 when it is
    /// degraded or down.
    Run {
        /// The `id` of the check to run.
        #[arg(value_name = "CHECK_ID")]
        check: String,

        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommand {
    /// Send one notifier a test alert, then its resolve, with no server
    /// and no database. Both are marked as tests. Exits 0 when both were
    /// accepted, 3 otherwise.
    Test {
        /// The `id` of the notifier to try.
        #[arg(value_name = "NOTIFIER_ID")]
        notifier: String,

        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum SlackCommand {
    /// Print the link that creates your own Slack app, its manifest filled
    /// in from the configuration, and what to do once it exists. Needs an
    /// `https` `[server] public_url`; works before `[slack]` is set.
    Setup {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate every `*.toml` file in the folder, reporting every error
    /// with its file, line and column. Exits 2 if any error is found.
    Check {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,

        /// Folder holding the SQLite database, read but never written, to
        /// warn about Slack connections notifiers name that are missing or
        /// dead. Defaults to `server.data_dir` from the configuration.
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
    },
    /// Print the merged configuration with defaults filled in and secrets
    /// redacted.
    Show {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print the JSON Schema that every configuration file follows.
    Schema,
    /// Ask a running server whether its configuration last reloaded. Exits
    /// 1 when the last reload failed and an older configuration is still
    /// running.
    Status {
        #[command(flatten)]
        server: ServerArgs,
    },
}

/// Where `tocsin` looks for configuration when `--config` is not given.
const DEFAULT_CONFIG: &str = "/etc/tocsin";
