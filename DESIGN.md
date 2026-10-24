# tocsin design

tocsin is an open-source, single-binary uptime monitor, status page and release log, written in Rust and configured entirely by files. It is built for Closient first and published for anyone.

This file is the decision record from the 2026-10-22 plan review, and it is canonical: change the design by changing this file in a pull request. It supersedes the February 2026 Go plan and the April 2026 Django plan, neither of which shipped.

## What makes it different

* **Files are the only configuration.** A folder of TOML files defines everything that is watched, how state is derived, who gets alerted and how the page looks. There is no UI and no API for changing configuration, and therefore no Terraform provider. Any tool that writes files (git, Ansible, an agent) is the IaC.
* **The database holds history, not configuration.** SQLite in a data folder stores check results, outage records, incidents, releases and, from v0.2, subscribers.
* **Agent-friendly end to end.** Configuring, running, interrogating and integrating must all be easy for a coding agent: schema-validated config, `--json` on every command, one-shot probe runs that explain their verdict, an OpenAPI spec, `/llms.txt`, and an MCP server.
* **Release log.** CI pushes each release and deploy (version, notes, time) to tocsin. tocsin keeps a public, subscribable log and shows deploys on the same timeline as outages, so "what changed right before this broke" is one glance.

## Decisions — Christian's calls (2026-10-22)

* **Language:** Rust.
* **Deployment:** one binary, plus a config folder and a data folder.
* **Topology:** single node first. A central reporting node or federation (nodes behind a firewall or DMZ reporting out, or nodes sharing data with each other) comes later.
* **Storage:** SQLite.
* **Configuration:** files only. The format is open as long as agents can write it and it can be verified (see the defaults below for the format chosen).
* **No Terraform provider.**
* **Incidents are data, not configuration.** They are stored in SQLite and written through `tocsin incident ...`, an authenticated write API, or a small admin page. They can be exported to markdown. Outages tocsin detects itself are recorded automatically. Scheduled maintenance follows the same rule: it is a kind of incident, not configuration.
* **Releases are push only.** CI posts the version, notes and deploy time. tocsin never polls GitHub, so there is no GitHub auth to manage.
* **Checks:** HTTP, TLS certificates, TCP and other standard network protocols, plus push heartbeats (for example, webhooks arriving).
* **Status page:** components, incidents, a 90-day uptime bar and live updates.
* **Notifications:** email, Slack, Pushover and more.
* **UI:** no UI for configuration. Beside the public status page there is a read-only internal board and the incident admin page (subscribers join it in v0.2), both behind the proxy auth below. The internal board shows every check and heartbeat, including those on no public component, such as machines, certificates and vendor status pages; it replaces the Gatus dashboard. It reads, never writes: a check is still changed only in its file. (Christian, 2026-10-23; this replaced "admin UI only for incidents and subscribers".)
* **Public and internal checks:** a check or heartbeat is public when it is on a component, since every component is on the public page; one on no component is internal. Nothing anonymous names an internal check, its state or its outages: not the read API, the event stream, the timeline, `/llms.txt`, the feeds, the status page or the MCP tools. The internal board and a `status:read` token see both. Outages follow a configured check to where it is now; a removed check's follow the component each recorded, so a check withdrawn from the status page keeps its public history. One predicate (`http::public`) decides it for every surface, and a test enumerates the router so a new anonymous route must be classified. (TOC-69)
* **Auth:** delegated to the proxy in front of tocsin (Cloudflare Access with Google login at Closient). tocsin does not run its own login system.
* **License:** AGPL-3.0. It stays truly open source, and anyone running a modified version as a service must publish their changes.
* **v0.1 scope:**
  * config and validation
  * every probe type, plus heartbeats
  * SQLite history
  * public status page
  * internal board of every check, behind proxy auth
  * incidents (CLI, API and admin page)
  * release log
  * operator alerts with resolve
  * `--json` CLI and `/metrics`

  v0.2 adds subscribers and MCP. Federation comes after that.

## Defaults (proposed in the review and not yet overruled; change them here)

* **Config format:** TOML, split across any number of `*.toml` files in the folder and merged, with IDs unique across files. The JSON Schema is generated from the Rust types (`schemars`) and published. `tocsin config check --json` reports every error with its file, line and column. Secrets come from environment variables (`${ENV:NAME}`) and never sit in files.
* **Reload:** tocsin watches the folder and also reloads on SIGHUP. A reload that fails validation keeps the old config running and reports the error through logs, `/metrics` and the API.
* **Assertions are structured TOML, not a string DSL.** They can be validated against the schema. Each assertion's failure maps to either `degraded` or `down`, which gives three health states: up, degraded, down.
* **API:**
  * Read side: status, history, incidents and releases, described by OpenAPI.
  * Write side: heartbeat pings, incidents, releases and deploys, and from v0.2 subscriber signup and unsubscribe.
  * Every write path uses a bearer token declared in config, whose secret comes from an environment variable.
* **Proxy auth:** tocsin verifies the `Cf-Access-Jwt-Assertion` token against the Cloudflare Access team's public keys (JWKS) and audience. It never trusts the email header on its own. Other proxies (oauth2-proxy, Tailscale) can use a generic trusted-header mode, which is off unless explicitly enabled.
* **Email:** the From and Reply-To addresses are required settings, and validation rejects any `noreply` address. Every email tocsin sends must be one a person can answer, matching Closient's own rule.
* **Federation groundwork, from day one:** every result carries a `node_id`, and results are append-only.
* **Operator alerts:** Slack, Pushover, email, ntfy, Discord, Telegram and a generic webhook, plus posting to Alertmanager. Every alert has a matching "resolved" message with the same identity, so no alert is fire-only.
* **Subscriber channels (v0.2):** email with double opt-in, Slack by signing in (below), plus RSS/Atom feeds for incidents and releases.
* **Agent surfaces:**
  * `--json` and stable exit codes on every command
  * `tocsin probe run <check>`
  * a built-in MCP server (v0.2)
  * `/llms.txt` and `/api/v1/status.json`
  * JSON-LD on the status page
  * `AGENTS.md` and markdown docs in the repo
* **Rust stack shortlist:**
  * tokio and axum for the runtime and web server
  * rusqlite (bundled) with `rusqlite_migration` for storage
  * askama and vendored htmx for the status page
  * reqwest with rustls for HTTP probes
  * hickory-resolver for DNS
  * surge-ping for ICMP
  * x509-parser for certificates
  * lettre for email
  * schemars, serde and toml for config
  * clap for the CLI
  * notify for file watching
  * jsonwebtoken for the Access JWT
  * utoipa for OpenAPI
  * prometheus-client for metrics
  * opentelemetry-proto (and, behind the `grpc` feature, tonic) for OTLP export; the export reads the same registry as `/metrics` (TOC-72)
  * tracing for logs
  * rmcp for MCP
  * cargo-dist for releases

## Durability (TOC-66)

Every write goes through the store's one writer thread, which commits in batches under WAL with `synchronous = NORMAL` ([docs/storage.md](docs/storage.md)). What "done" means to the caller depends on the write:

* **A check's probe result that changes nothing** (it opens and ends no outage) is acknowledged once the writer's queue has taken it, before it commits. It is the only write acknowledged early. It is almost every write, and losing one costs the least: the next probe of the same check says the same thing.
* **Everything else is committed before it is acknowledged:** every state change (an outage opening or ending, with the result that confirmed it), every alert and its deliveries, every heartbeat ping and missed deadline, every incident and its updates, every release and deploy, retention's writes, and every `Store::append`. A 2xx from the API for any of these means its rows are in a committed transaction.

**What a process crash loses** (a kill, the OOM killer, an abort): no-change probe results only, and at most one queue's worth of them. That is the 1024 the queue holds plus the batch of up to 256 the writer has taken but not yet committed: 1280 at most. Nothing that changed a check's state is lost, and neither is any alert, heartbeat, incident or release. The queue is first in, first out, so a change queued after a no-change result never commits before it. `Store::close` waits for the writer to commit everything queued, and `serve` closes the store on shutdown, so a clean shutdown loses nothing. Dropping a `Store` without closing it does not wait: its writer thread still drains the queue, but only while the process lives.

**A power cut** loses what it lost before this rule: under `synchronous = NORMAL` the last few commits of any kind, never the file's integrity.

**A full queue makes the caller wait** rather than drop the result, the same backpressure every other write has.

**A refused result is logged and counted.** If the database refuses a result that was already acknowledged (a full disk, a constraint), the writer logs it at `error` with its check, node and start time, and counts it in `CheckpointStats::rejected`. The engine has moved on by then: the result still counts towards confirming a change, but the change itself is committed before it moves any state, so a change the database refuses is retried by the next result that confirms it.

**Why early acknowledgement, not a faster commit.** SQLite syncs the log's header when a commit rewinds the log to its start (`sqlite3WalFrames`: `if( pWal->syncHeader ){ rc = sqlite3OsSync(pWal->pWalFd, CKPT_SYNC_FLAGS(sync_flags)); }`, bundled SQLite 3.53.2). That sync runs inside the commit, on the writer, holding the write lock, at any `synchronous` level above OFF. Only a VFS reporting `SQLITE_IOCAP_SEQUENTIAL` skips it. On a slow disk it took hundreds of milliseconds, and every check result queued behind it waited too. The ways to keep it off a commit (`synchronous = OFF`, a VFS lying about the device, a VFS written in Rust) give up durability or `unsafe`-free code. So the sync stays and the caller stops waiting for it, for the one write whose loss is cheap. The write-latency bench still reports how long every insert waits from queue to commit, so a slow disk stays visible.

## Slack by signing in (TOC-68)

**Each operator registers their own Slack app.** There is no shared tocsin (or Closient) app on Slack and no relay between a tocsin server and Slack. `tocsin slack setup` prints Slack's create-app link with a manifest filled in from the configuration: the redirect URL from `public_url`, and the one scope `incoming-webhook`. The app's Client ID and Secret go in `[slack]` as `${ENV:...}` references, never in a file.

Why not one shared app: a shared app needs a server that every tocsin's sign-in passes through, since Slack allows only registered redirect URLs. That relay would see every workspace's webhook, would be a service someone must run for every self-hoster, and would put Closient between other people's status pages and their users. A per-operator app costs each operator one link and two environment variables, and keeps every webhook on the server that uses it.

* **Connections are data.** A connected channel is a row in SQLite, made on `/admin/slack` with "Connect Slack" (operators) or "Add to Slack" on the subscribe page (visitors), never written in configuration. A notifier names one with `[notifier.slack] connection = "..."`.
* **The OAuth `state` is session-bound, short-lived and single-use**: a random value tied to the starting browser by a `__Host-` cookie, valid for 10 minutes, removed on first use.
* **The webhook URL is a secret**: never logged, never shown after connect, never in the API, `/metrics` or an error.
* **Nothing fails quietly.** A connection named in config but not connected is a warning from `config check` (it can only be made on a running server) and, at send time, an abandoned delivery, an `error` log and `tocsin_slack_connection_up` at 0. A connection Slack revokes is marked dead, on the admin page and in `/metrics`, and is not retried.
* **Subscribers get Slack beside email**, with the same preferences. There is no double opt-in: Slack's own consent screen proves the visitor may post to the channel. Every message carries the manage and remove links and the contact address.

## Milestones

* **v0.1 — Monitor, page and releases.** Enough to retire Gatus at Closient. UptimeFlare stays as the outside watchdog for the tocsin node, because a single node cannot report its own death.
* **v0.2 — Subscribers and MCP.**
* **Federation.** Design first, then build.

## Repo

The repository was relicensed from MPL-2.0 to AGPL-3.0-only before any outside contribution, so the AGPL applies to every line of tocsin.
