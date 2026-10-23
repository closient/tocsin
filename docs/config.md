# Configuration

tocsin is configured by a folder of TOML files and nothing else. Every `*.toml` file under the folder is read, including subfolders, in sorted path order, and the files are merged into one configuration. Split them however suits you: one file per team, per service, or per kind of thing. [`examples/config/`](../examples/config/) is a complete working example.

```sh
tocsin config check --config /etc/tocsin           # validate; exit 2 on any error
tocsin config check --config /etc/tocsin --json    # the same, as one JSON document
tocsin config show --config /etc/tocsin --json     # the merged result, secrets redacted
tocsin config schema > tocsin.schema.json          # the JSON Schema for one file
```

`--config` defaults to `/etc/tocsin` for every command, `tocsin serve` included. `tocsin serve` loads the same folder and refuses to start (exit 2) if it does not validate. A folder with no `*.toml` file is an error (exit 2), not an empty configuration.

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
/etc/tocsin/checks.toml:5:1: unknown field `intervall`, expected one of `id`, `name`, `component`, `type`, `target`, `interval`, `timeout`, `confirm`, `http`, `dns`, `tls`, `assert` (check[0].intervall)
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

## Conventions

- **Ids** are slugs: lowercase letters, digits and hyphens, starting with a letter or digit (`^[a-z0-9][a-z0-9-]*$`). An id is unique among items of its kind across every file: a check and a component may share the id `api`, but two components may not. A duplicate is reported with the location of the first definition.
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
| `public_url` | URL | none | Where people reach the status page; used in links and emails. `http://` or `https://`. |
| `node_id` | string | the hostname | This node's name in every result it records. No whitespace. |
| `data_dir` | path | none | Folder for the SQLite database. A relative path is relative to the config folder. `tocsin serve --data` overrides it; one of the two is required. |
| `retention_days` | integer | `90` | Whole UTC days of raw check results kept before today; daily rollups are kept forever. At least 1. See [storage.md](storage.md#retention). |

### `[server.email]`

Required, with all three keys, once any notifier has `type = "email"`.

| Key | Type | |
|-----|------|-|
| `from` | address | The From address, as `status@example.com` or `Example Status <status@example.com>`. |
| `reply_to` | address | The Reply-To address: a mailbox a person reads. |
| `smtp_url` | secret | `smtp://user:password@host:587` or `smtps://...`; any other scheme is an error. |

Every email tocsin sends can be answered. An address whose local part contains `noreply` or `donotreply` is rejected in either field, ignoring case, `+tags` and `-`, `_` or `.` separators: `No.Reply+alerts@`, `no-reply-status@` and `alerts-noreply@` are all rejected.

### `[server.auth]`

Reserved for the admin page. It accepts no settings yet.

## `[page]`

| Key | Type | Default | |
|-----|------|---------|-|
| `title` | string | `"Status"` | The page title. |
| `description` | string | none | A line under the title. |
| `homepage_url` | URL | none | Where the logo and title link to. |

## `[[group]]` and `[[component]]`

Components are what the status page shows, each with a health state. Groups are headings that collect them.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. |
| `name` | string | Required. Display name. |
| `description` | string | Optional. |
| `group` | group id | Components only: the group it is shown under. |

## `[[check]]`

A probe run on an interval.

| Key | Type | Default | |
|-----|------|---------|-|
| `id` | id | | Required. |
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
| `headers` | `{}` | Request headers, as a table. Each value **may be secret**. |
| `body` | none | Request body. **May be secret.** |
| `verify_certificate` | `true` | Reject invalid TLS certificates. |

`[check.dns]`: `record_type` (`A` by default; also `AAAA`, `CNAME`, `MX`, `NS`, `TXT`, `SRV`, `CAA`) and `resolver`, an IP address as `ip`, `ip:port` or `[ipv6]:port` (the system resolver by default; a hostname is an error).

`[check.tls]`: `server_name`, the SNI hostname to send when it differs from the target host.

### Assertions: `[[check.assert]]`

Each assertion holds exactly one condition. If it fails, the check is `down`, or `degraded` with `on_fail = "degraded"`.

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
| `body` | http | Exactly one of `contains`, `not_contains`. |
| `header` | http | `name` plus exactly one of `eq`, `contains`, `exists`. Names match without regard to case. |
| `certificate_days_remaining` | http, tls | Bounds, as `latency_ms`. |
| `records` | dns | `contains`: one returned record equals this. |

A JSON `path` starts with `$`, then any of `.key`, `[index]`, `["key"]` or `['key']`: `$.checks[0].status`.

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

A ping tocsin expects to receive, such as a cron job reporting in. Pings arrive through the write API with a token that has the `heartbeat` scope.

| Key | Type | Default | |
|-----|------|---------|-|
| `id` | id | | Required. |
| `name` | string | the id | |
| `component` | component id | none | The component whose state this heartbeat sets. |
| `interval` | duration | | Required. How often a ping is expected. At least `1s`. |
| `grace` | duration | `"60s"` | How late a ping may be before the heartbeat is down. |

A heartbeat takes `[[heartbeat.link]]` tables, the same as [check links](#links-checklink-and-heartbeatlink).

## `[[project]]`

Something whose releases and deploys the release log records. CI pushes them; tocsin never polls.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. |
| `name` | string | Required. |
| `description` | string | Optional. |
| `url` | URL | Optional. The homepage or repository. |
| `component` | component id | Optional. Its timeline shows this project's deploys. |

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
| `pushover` | `user_key` (secret), `api_token` (secret) |
| `email` | `to`: a list of addresses. Sender settings come from `[server.email]`. |
| `ntfy` | `url` (the topic URL), `token` (secret, optional) |
| `discord` | `webhook_url` (secret) |
| `telegram` | `bot_token` (secret), `chat_id` |
| `webhook` | `url`, `bearer_token` (secret, optional) |
| `alertmanager` | `url` (the Alertmanager base URL), `bearer_token` (secret, optional) |

The settings table for the notifier's type is required, and a table for any other type is an error.

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

## `[[token]]`

A bearer token for the write API.

| Key | Type | |
|-----|------|-|
| `id` | id | Required. Logged with each write. |
| `secret` | secret | Required. Two tokens may not resolve to the same value; the error names both tokens, never the value. |
| `scopes` | list of `heartbeat`, `incident`, `release` | Required, at least one. |
| `description` | string | Optional. Who or what uses the token. |
