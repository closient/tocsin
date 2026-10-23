# Configuration

tocsin is configured by a folder of TOML files and nothing else. Every `*.toml` file under the folder is read, including subfolders, in sorted path order, and the files are merged into one configuration. Split them however suits you: one file per team, per service, or per kind of thing. [`examples/config/`](../examples/config/) is a complete working example.

```sh
tocsin config check --config /etc/tocsin           # validate; exit 2 on any error
tocsin config check --config /etc/tocsin --json    # the same, as one JSON document
tocsin config show --config /etc/tocsin --json     # the merged result, secrets redacted
tocsin config schema > tocsin.schema.json          # the JSON Schema for one file
```

`--config` defaults to `/etc/tocsin` for every command, `tocsin serve` included. `tocsin serve` loads the same folder and refuses to start (exit 2) if it does not validate, then [reloads it](#reloading) as it changes. A folder with no `*.toml` file is an error (exit 2), not an empty configuration.

Hidden files and folders (names starting with `.`) are skipped. A symlinked file is read; a symlinked folder is not followed.

## Editor support

`tocsin config schema` prints a JSON Schema (draft 7) describing one file. The repository commits it as [`schema/tocsin.schema.json`](../schema/tocsin.schema.json). Point your editor at it with a directive on the first line of each file, which [taplo](https://taplo.tamasfe.dev/) and the Even Better TOML extension for VS Code read:

```toml
#:schema ./tocsin.schema.json
```

The path is relative to the file. The schema checks the shape of each file: field names, types, enums and formats. Rules that span files, such as references between ids, are only checked by `tocsin config check`.

## Errors

`tocsin config check` reports every problem it finds rather than stopping at the first, each located by file, 1-based line and 1-based column (counted in characters):

```text
error: configuration in /etc/tocsin has 2 errors
/etc/tocsin/checks.toml:5:1: unknown field `intervall`, expected one of `id`, `name`, `component`, `type`, `target`, `interval`, `timeout`, `confirm`, `http`, `dns`, `tls`, `tcp`, `assert`, `link` (check[0].intervall)
  hint: did you mean `interval`?
/etc/tocsin/checks.toml:9:13: unknown component `apj` (check[1].component)
  hint: did you mean `api`?
```

With `--json`, success is `{"ok": true, "files": [...], "counts": {...}}`, and failure (exit 2) is:

```json
{"error": {"kind": "invalid_input", "message": "configuration in /etc/tocsin has 2 errors",
           "errors": [{"file": "/etc/tocsin/checks.toml", "line": 5, "column": 1,
                       "path": "check[0].intervall", "message": "...", "hint": "did you mean `interval`?"}]}}
```

Every error has all six keys; `line`, `column`, `path` and `hint` are `null` when unknown. `path` names the value in the file's data, such as `check[2].assert[0].latency_ms`, where `check[2]` is the third `[[check]]` in that file.

Each `[[check]]`, `[[component]]` and other item is read on its own, so one broken item does not hide problems in the others. Within one item, a value that cannot be read at all (an unknown field, a wrong type, a bad duration) is reported, and that item's other rules are not checked until it is fixed.

A file with a TOML syntax error, or one that cannot be read or is not UTF-8, is reported and otherwise skipped. The ids it defines are then unknown, so unknown references are not reported until it is fixed, rather than reporting every reference to an id in it.

## Reloading

`tocsin serve` reloads the folder by itself half a second after the last change to a `*.toml` file or a subfolder in it, and at once on SIGHUP (`systemctl reload tocsin`). Changes to hidden files, such as an editor's swap files, are ignored. A Kubernetes `ConfigMap` mounted as the folder reloads when Kubernetes swaps in a new version. If the folder cannot be watched, the log says so and only SIGHUP reloads it.

A reload reads and validates the whole folder exactly as startup does, and either takes all of it or none of it:

- **A configuration that validates** goes in at once. Checks and heartbeats it adds start; ones it removes stop; ones it changes restart with their state, so a down check stays down and its outage stays open. Unchanged ones keep running untouched. Notifiers, routes, components, projects, tokens, `[page]` and everything else served over HTTP take effect for the next request. Every result already recorded stays in history, removed checks' included.
- **A configuration that does not validate changes nothing**: the last good one keeps running, and the reason shows in four places: the log (`configuration reload failed`, with every error), the metrics `tocsin_config_reload_success` (0) and `tocsin_config_last_error_timestamp`, `GET /api/v1/config/status`, and `tocsin config status`. An `${ENV:...}` naming a variable that is not set is an invalid configuration like any other.
- **`[server]`** is read only at startup: it names the socket, the database and the node the history belongs to. A reload keeps the running `[server]` and logs a warning if the files say something else; restart to apply it. The environment is likewise read once, so a new secret needs a restart.

Removing something that is alerting ends it rather than leaving it open:

- **A removed check or heartbeat** with an open outage has the outage withdrawn: ended at the reload, with no resolve sent, since nobody saw it recover, and recorded as `ending: "withdrawn"` in the read API ([api.md](api.md)). An alert still ringing (a Pushover emergency) is cancelled.
- **A removed notifier** has its open alerts ended, with nothing sent, since there is nothing left to send through.
- **A changed route** applies to outages opened from then on; an alert already firing keeps the notifiers it fired on.

The same happens at startup, for whatever the configuration dropped while tocsin was stopped.

```sh
tocsin config status --json    # exit 0 if the last reload succeeded, 1 if it failed
```

```json
{"loaded_at": "2026-10-23T09:12:03.120Z", "last_attempt_at": "2026-10-23T09:40:51.007Z", "ok": false,
 "errors": [{"file": "/etc/tocsin/tokens.toml", "line": 4, "column": 10, "path": "token[0].secret",
             "message": "environment variable `DEPLOY_TOKEN` is not set",
             "hint": "set DEPLOY_TOKEN in the environment tocsin runs in"}]}
```

`loaded_at` is when the running configuration was read; `last_attempt_at` is the latest reload, successful or not; `errors` has the shape `tocsin config check --json` reports. `tocsin config status` asks a running server (`--url` or `TOCSIN_URL`) and needs a token with `status:read` (`--token` or `TOCSIN_TOKEN`), as does the endpoint: the errors name files and environment variables.

## Conventions

- **Ids** are slugs: lowercase letters, digits and hyphens, starting with a letter or digit (`^[a-z0-9][a-z0-9-]*$`). An id is unique among items of its kind across every file: a check and a component may share the id `api`, but two components may not, and neither may a check and a heartbeat (see [heartbeats](heartbeats.md)). A duplicate is reported with the location of the first definition.
- **References** name an id defined in any file. An unknown id is an error, with the nearest defined id as a hint.
- **Durations** are strings of number-and-unit parts: `500ms`, `30s`, `5m`, `1h30m`, `1d`.
- **Unknown fields are errors**, so a misspelled setting never silently does nothing.
- **`[server]` and `[page]`** may each appear in only one file. Every other table is an array of tables (`[[check]]`, not `[check]`) and may appear in any number of files.

## Secrets

Fields marked **secret** below take only an environment reference, never the value itself:

```toml
webhook_url = "${ENV:TOCSIN_SLACK_WEBHOOK_URL}"
```

- A secret field holding anything else is an error, and the error does not repeat what was written, in case it was a pasted credential.
- An unset or empty variable is an error naming the variable.
- `${ENV:...}` anywhere other than a secret field is an error: it is never expanded there.
- A reference is the whole value: `"Bearer ${ENV:API_TOKEN}"` is an error, and `API_TOKEN` holds `Bearer ...` instead.
- `tocsin config show` and every log print secrets as `<redacted from ${ENV:NAME}>`.

Fields marked **may be secret** take either a value written in the file or an environment reference.

## `[server]`

| Key | Type | Default | |
|-----|------|---------|-|
| `listen` | `"ip:port"` | `"127.0.0.1:8080"` | Address the web server listens on: an IP address, not a hostname, such as `"0.0.0.0:8080"` or `"[::]:8080"`. |
| `public_url` | URL | none | Where people reach the status page; used in links and emails. `http://` or `https://`. Required when `[server.auth]` is on. |
| `node_id` | string | the hostname | This node's name in every result it records. No whitespace. |
| `data_dir` | path | none | Folder for the SQLite database. A relative path is relative to the config folder. `tocsin serve --data` overrides it; one of the two is required. |
| `retention_days` | integer | `90` | Whole UTC days of raw check results kept before today; daily rollups are kept forever. At least 1. See [storage.md](storage.md#retention). |
| `max_concurrent_checks` | integer | `64` | Most probes running at once across all checks; the rest wait for a slot. At least 1. See [engine.md](engine.md#scheduling). |

### `[server.email]`

Required, with all three keys, once any notifier has `type = "email"`.

| Key | Type | |
|-----|------|-|
| `from` | address | The From address, as `status@example.com` or `Example Status <status@example.com>`. |
| `reply_to` | address | The Reply-To address: a mailbox a person reads. |
| `smtp_url` | secret | `smtp://user:password@host:587` or `smtps://...`; any other scheme is an error. |

Every email tocsin sends can be answered. An address whose local part contains `noreply` or `donotreply` is rejected in either field, ignoring case, `+tags` and `-`, `_` or `.` separators: `No.Reply+alerts@`, `no-reply-status@` and `alerts-noreply@` are all rejected.

### `[server.auth]`

Turns on the admin page at `/admin/` by naming the proxy that signs people
in. Without it, or with `mode = "none"`, `/admin/` does not exist. Setup and
the threat model are in [admin.md](admin.md).

| Key | Type | Default | |
|-----|------|---------|-|
| `mode` | `none`, `cloudflare_access`, `trusted_header` | `"none"` | How tocsin learns who is asking. |
| `team_domain` | string | | `cloudflare_access`, required: the Zero Trust team, as `example` or `example.cloudflareaccess.com`. |
| `audience` | string | | `cloudflare_access`, required: the Access application's AUD tag. |
| `header` | header name | `"X-Forwarded-Email"` | `trusted_header`: the header holding the email address. |
| `trusted_proxies` | list of CIDRs | | `trusted_header`, required: the networks the proxy connects from, such as `["10.0.0.5/32", "fd00::/64"]`. The header is believed only when the TCP peer is inside one; `X-Forwarded-For` is never read. |
| `allowed_emails` | list of addresses | everyone | Addresses allowed in, ignoring case. |
| `allowed_domains` | list of domains | everyone | Domains allowed in, matched exactly: `example.com` does not admit `team.example.com`. |

With both allow-lists empty, everyone the proxy vouches for is allowed;
otherwise an address must be on `allowed_emails` or at a domain on
`allowed_domains`. A key that does not apply to the chosen `mode` is an
error, so a forgotten `mode` line cannot leave the page silently off.

`cloudflare_access` checks the token Access signs, never the
`Cf-Access-Authenticated-User-Email` header: anything that reaches tocsin
without passing through Access could send that header.

Any `mode` other than `none` requires `[server] public_url`: the admin
forms are accepted only from its origin (scheme, host and port), never by
comparing against the request's `Host`.

## `[page]`

How the public status page looks; what it shows is in
[status-page.md](status-page.md).

| Key | Type | Default | |
|-----|------|---------|-|
| `title` | string | `"Status"` | The page title. |
| `description` | string | none | A line under the title. |
| `homepage_url` | URL | none | Where the logo and title link to. |
| `groups` | list of group ids | definition order | The order groups are shown in. Groups not listed follow in the order they are defined. |
| `components` | list of component ids | definition order | The order components are shown in within their group. |
| `timezone` | IANA name | `"UTC"` | The time zone the page shows times in. The uptime bars' days are UTC days regardless. |
| `[page.logo]` | table | none | `url`, a path on this site, and `alt`, its text. |
| `[[page.link]]` | tables | none | `name` and `url`: links in the page's header, such as the main site or a support address. |

```toml
[page]
title = "Example status"
description = "Live status of the Example API and website."
homepage_url = "https://example.com"
groups = ["edge", "core"]
components = ["web", "api"]
timezone = "Europe/Berlin"

[page.logo]
url = "/brand/logo.svg"
alt = "Example"

[[page.link]]
name = "Support"
url = "mailto:support@example.com"
```

Each id in `groups` and `components` must name a `[[group]]` or
`[[component]]`, once. The logo must be a path on this site, served by the
proxy in front of tocsin: the page's content security policy loads images
from its own origin only, so a URL on another host would not show.

## `[[group]]` and `[[component]]`

Components are what the status page shows, each with a health state. Groups are headings that collect them.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. |
| `name` | string | Required. Display name. |
| `description` | string | Optional. |
| `group` | group id | Components only: the group it is shown under. |
| `rule` | `"worst"` or `"majority"` | Components only. How its checks combine into its health: the worst check (the default), or the worst state more than half of them are in or beyond. See [engine.md](engine.md#component-status). |

## `[[check]]`

A probe run on an interval.

| Key | Type | Default | |
|-----|------|---------|-|
| `id` | id | | Required. Unique across checks and heartbeats. |
| `name` | string | the id | Display name. |
| `component` | component id | none | The component whose state this check sets. |
| `type` | `http`, `tcp`, `dns`, `icmp`, `tls` | | Required. |
| `target` | string | | Required. See below. |
| `interval` | duration | `"60s"` | Time between probes. At least `1s`. |
| `timeout` | duration | `"10s"` | Must be shorter than `interval`. |
| `confirm` | integer | `1` | Consecutive results needed before the state changes. At least 1. |

`target` by type:

| `type` | `target` | Example |
|--------|----------|---------|
| `http` | an `http://` or `https://` URL | `"https://api.example.com/health"` |
| `tcp` | `host:port` | `"db.example.com:5432"`, `"[2001:db8::1]:443"` |
| `tls` | `host` or `host:port` (default 443) | `"api.example.com"` |
| `dns` | a domain name | `"example.com"` |
| `icmp` | a host or IP address | `"192.0.2.1"` |

Type-specific settings go in a table named after the type; each is optional and only allowed on checks of that type.

`[check.http]`:

| Key | Default | |
|-----|---------|-|
| `method` | `"GET"` | `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` or `OPTIONS`. |
| `follow_redirects` | `false` | Follow 3xx responses. |
| `headers` | `{}` | Request headers, as a table. Each value **may be secret**. Loading rejects a name that is not an HTTP header name, and a value, a secret's included, that holds a control character such as a newline or a character outside ASCII; a secret's value is never shown, only its variable. |
| `body` | none | Request body. **May be secret.** |
| `verify_certificate` | `true` | Treat a certificate that does not verify as a failed probe (`down`). With `false` the request goes ahead and the certificate is still reported, so `chain_valid` and `hostname_valid` assertions can judge it. |
| `server_name` | none | The hostname for SNI, the `Host` header and the certificate check, when it differs from the target host. The connection still goes to the target host's address. |
| `bearer_token` | none | Sent as `Authorization: Bearer <token>`. **Secret.** |
| `basic_auth` | none | `{ username = "...", password = "${ENV:...}" }`, sent as HTTP basic authentication. `username` **may be secret**; `password` is **secret**. Give `bearer_token` or `basic_auth`, not both. |

An `http` probe reads at most 64 KiB of the response body; assertions see that much, and the result says when the body was cut. Proxy environment variables are ignored: a probe measures the target, not a proxy.

`[check.dns]`: `record_type` (`A` by default; also `AAAA`, `CNAME`, `MX`, `NS`, `TXT`, `SRV`, `CAA`) and `resolver`, an IP address as `ip`, `ip:port` or `[ipv6]:port` (the system resolver by default; a hostname is an error). Each probe asks once, with no cache and without the hosts file, and the target is a complete name: no search domains are tried.

`[check.tls]`: `server_name`, the SNI hostname to send, and to check the certificate against, when it differs from the target host.

`[check.tcp]`: `send`, text to write once connected, such as `"PING\r\n"`. The reply, or the server's greeting when nothing is sent, is read only when a `banner` assertion asks for it; see [probes](probes.md#tcp-banners).

### Assertions: `[[check.assert]]`

Each assertion holds exactly one condition. If it fails, the check is `down`, or `degraded` with `on_fail = "degraded"`. A check's health is the worst of its failing assertions: `up` when every one passes, `down` when any failing one is `down`, `degraded` otherwise. A probe that cannot complete (a timeout, a refused connection, a failed lookup, a certificate that does not verify) is `down` whatever its assertions say.

```toml
[[check.assert]]
status = [200, 204]

[[check.assert]]
latency_ms = { lt = 800 }
on_fail = "degraded"

[[check.assert]]
json = { path = "$.status", eq = "ok" }
```

| Condition | Applies to | Form |
|-----------|-----------|------|
| `status` | http | A list of acceptable status codes, 100 to 599. |
| `latency_ms` | every type | Bounds: any of `lt`, `lte`, `gt`, `gte`, `eq`; every bound given must hold. |
| `json` | http | `path` plus exactly one of `eq`, `ne` (any value), `exists` (bool), `contains` (string). |
| `body` | http | Exactly one of `contains`, `not_contains`, `regex`. |
| `header` | http | `name` plus exactly one of `eq`, `contains`, `exists`. Names match without regard to case. |
| `certificate_days_remaining` | http (https only), tls | Bounds, as `latency_ms`. |
| `hostname_valid` | http (https only), tls | `true` or `false`: whether the certificate names the host. |
| `chain_valid` | http (https only), tls | `true` or `false`: whether the chain verifies against the system's roots and is in date. |
| `records` | dns | Exactly one of `contains` (one returned record equals this) and `eq` (a list: the returned records are exactly these, in any order). IP addresses compare in canonical form, and names without the trailing dot. CNAME, MX, NS and SRV records ignore case; TXT and CAA text does not. |
| `banner` | tcp | Exactly one of `contains`, `not_contains`, `regex`, matched against the reply. |

`regex` is [Rust `regex` syntax](https://docs.rs/regex/latest/regex/#syntax) and matches anywhere unless anchored with `^` and `$`. Patterns, like JSON paths, are checked when the configuration loads.

A JSON `path` is an [RFC 9535](https://www.rfc-editor.org/rfc/rfc9535) JSONPath: `$.checks[0].status`, or with a filter, `$.components[?@.name=='API'].status`. The assertion compares the first node the path selects, so a filter that matches one item reads like gjson's `components.#(name=="API").status`. A path that selects nothing fails every matcher except `exists = false`. RFC 9535 dot notation takes only names that start with a letter or `_` and hold letters, digits and `_`; any other name needs bracket notation, `$['x-request-id']`, and loading says so.

```toml
# An Atlassian Statuspage component, as vendor status pages publish them.
[[check.assert]]
json = { path = "$.components[?@.name=='API Requests'].status", eq = "operational" }
on_fail = "degraded"
```

A check with no assertions gets implicit ones, reported with `"implicit": true`: an `http` response's status must be 2xx or 3xx, a `dns` answer's response code must be `NOERROR`, and a `tls` certificate must have a valid chain and name the host. `tcp` and `icmp` checks with no assertions pass when the probe completes.

### Links: `[[check.link]]` and `[[heartbeat.link]]`

Named links to where an operator looks next, such as a dashboard, logs or a runbook, shown with the check or heartbeat on the internal board. Each needs both keys:

| Key | Type | |
|-----|------|-|
| `name` | string | Required, not blank. The link text. |
| `url` | URL | Required. An absolute `http://` or `https://` URL. |

```toml
[[check.link]]
name = "Runbook"
url = "https://wiki.example.com/runbooks/api"
```

## `[[heartbeat]]`

A ping tocsin expects to receive, such as a cron job reporting in. Pings arrive through the write API with a token that has the `heartbeat` scope; [`heartbeats.md`](heartbeats.md) has the endpoint and the deadline rules.

| Key | Type | Default | |
|-----|------|---------|-|
| `id` | id | | Required. Unique across checks and heartbeats. |
| `name` | string | the id | |
| `component` | component id | none | The component whose state this heartbeat sets. |
| `interval` | duration | | Required. How often a ping is expected. At least `1s`. |
| `grace` | duration | `"60s"` | How late a ping may be before the heartbeat is down: it is missed once `interval + grace` passes after its last result. |

A heartbeat takes `[[heartbeat.link]]` tables, the same as [check links](#links-checklink-and-heartbeatlink).

`interval + grace` must be at most half of `[server] retention_days`, so retention never deletes the result a deadline runs from; see [`heartbeats.md`](heartbeats.md#configuration).

## `[[project]]`

Something whose releases and deploys the release log records. CI pushes them; tocsin never polls.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. |
| `name` | string | Required. |
| `description` | string | Optional. |
| `url` | URL | Optional. The homepage or repository. |
| `components` | list of component ids | Optional. The components this project's deploys belong to: a timeline filtered to one of them shows them. |
| `public` | bool | Default `true`. Whether `/releases`, `/releases.atom` and the read API without a token show its releases and deploys. A non-public project's releases are seen only with a `status:read` token or on `/admin/timeline`. See [releases.md](releases.md). |

## `[[notifier]]`

A destination for operator alerts. `type` names the service, and its settings go in the table of the same name:

```toml
[[notifier]]
id = "ops-slack"
type = "slack"

[notifier.slack]
webhook_url = "${ENV:TOCSIN_SLACK_WEBHOOK_URL}"
```

Every notifier has an `id`, an optional display `name`, and a `type`.

| `type` | Settings |
|--------|----------|
| `slack` | `webhook_url` (secret) |
| `pushover` | `user_key` (secret), `api_token` (secret), and the priorities `down_priority` (default 1), `degraded_priority` (0) and `resolved_priority` (0), each -2 to 2. 2 is an emergency, repeated until acknowledged, and cancelled when the alert resolves or drops to a priority below 2. |
| `email` | `to`: a list of addresses. Sender settings come from `[server.email]`; `smtp_url` is `smtps://` for TLS, or `smtp://`, which requires STARTTLS unless it ends `?tls=none`. |
| `ntfy` | `url` (the topic URL), `token` (secret, optional) |
| `discord` | `webhook_url` (secret) |
| `telegram` | `bot_token` (secret), `chat_id` |
| `webhook` | `url`, `signing_secret` (secret, required: each body is signed with HMAC-SHA256; generate one with `openssl rand -hex 32`), `bearer_token` (secret, optional) |
| `alertmanager` | `url` (the Alertmanager base URL), `bearer_token` (secret, optional) |

`max_in_flight` (default 2, at least 1) is the most deliveries the notifier sends at once; the rest wait their turn.

The settings table for the notifier's type is required, and a table for any other type is an error. What each type sends, and how to verify a webhook's signature, is in [alerting.md](alerting.md). `tocsin notify test <id>` sends a notifier a test alert and its resolve.

## `[[route]]`

Sends alerts matching its filters to its notifiers. An empty filter matches everything.

| Key | Type | Default | |
|-----|------|---------|-|
| `id` | id | | Required. |
| `notifiers` | notifier ids | | Required, at least one. |
| `components` | component ids | `[]` | Only alerts for these components. |
| `groups` | group ids | `[]` | Only alerts for components in these groups. |
| `checks` | check ids | `[]` | Only alerts for these checks. |
| `heartbeats` | heartbeat ids | `[]` | Only alerts for these heartbeats. |
| `notify_on` | `degraded` or `down` | `down` | The least severe state that alerts. |
| `repeat` | duration | `"4h"` | Re-send while the alert is still firing. At least `"1m"`, or `"0s"` to never repeat. |

When several routes send one alert to the same notifier, it is sent once and repeats on the shortest of their intervals. Every alert is resolved; there is no `resolve` key, and a route that sets one is an error.

## `[[token]]`

A bearer token for the API: writing incidents, releases and heartbeats, and reading what is not public.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. Logged with each write. |
| `secret` | secret | Required. Two tokens may not resolve to the same value; the error names both tokens, never the value. |
| `scopes` | list of `heartbeat`, `incident`, `release`, `status:read` | Required, at least one. `release` publishes releases and stamps deploys; `status:read` reads what the public cannot: non-public projects' releases and deploys, check targets, raw check results, and [the reload status](#reloading). See [api.md](api.md#who-sees-what). |
| `description` | string | Optional. Who or what uses the token. |
