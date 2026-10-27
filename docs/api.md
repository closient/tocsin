# Read API

What a running `tocsin serve` says about itself: the status now, each
component and check, raw check results, outages, incidents, releases and
the timeline. Writes are documented with their models in
[incidents.md](incidents.md#http-api) and [releases.md](releases.md#http-api),
and the subscriber routes in [subscribers.md](subscribers.md).

The OpenAPI 3.1 document is served at `GET /api/v1/openapi.json` and
committed as [`schema/openapi.json`](../schema/openapi.json); it is the
reference for every field. This page says what each endpoint is for and
who may read it.

## Who sees what

Public data needs no token: the status page shows it anyway. Send
`Authorization: Bearer <token>` with a token whose `[[token]]` entry has
the `status:read` scope to read the rest:

| Data | Without a token | With `status:read` |
|---|---|---|
| Overall status, components, their health, open incidents | yes | yes |
| Checks and heartbeats on a component: id, kind, name, interval, health, outages | yes | yes |
| A component with `public = false`, its checks and heartbeats, their changes, outages and timeline | no | yes |
| Checks and heartbeats on no component, their changes and their outages | no | yes |
| A check's `target` (its URL or host, which can name an internal host) | no | yes |
| A check's or heartbeat's `topology_node` (which can name an internal host) | no | yes |
| A check's raw results (`/checks/{id}/results`) | no, 401 | yes |
| How the configuration last reloaded (`/config/status`), whose errors name files and environment variables | no, 401 | yes |
| Prometheus metrics (`/metrics`, see [metrics.md](metrics.md)) | no, 401 | yes |
| Incidents | yes | yes |
| Releases and deploys of a project with `public = true` | yes | yes |
| Releases and deploys of a project with `public = false` | no | yes |

One rule holds on every read, `/llms.txt` included:

- No `Authorization` header: the public view.
- A known token with `status:read`: the full view.
- A known token without it (an `incident` or `release` token, say): the
  public view, never 403, so a write token left in `TOCSIN_TOKEN` does not
  break a read.
- A header that is malformed or names no `[[token]]`: 401, so a typo is
  loud rather than quietly anonymous.

Raw check results, the configuration status and `/metrics` are the reads
with nothing public to show: no token is 401 and a token without
`status:read` is 403.

### Public and internal checks

A check or heartbeat is public when it is on a public component, the
default. One on no component is internal: the
[internal board](admin.md#the-internal-board) and a `status:read` token
see it, and without that scope no read names it, its health or its
outages. `checks`, `events`, `outages`, `timeline` and the MCP tools
leave it out wholesale; `status.json`, `components/{id}`, `/llms.txt`,
the feeds and the status page never listed it, and a component's health
comes from its own checks only. A configured check's outages follow
where it is now, so moving it off its component hides its old outages
too. A removed check has only its outages to go by: those recorded on a
public component stay public, ending `withdrawn`, and those recorded on
none, or on a component that is not public, stay hidden.

A `[[component]]` with `public = false` is internal too, and so is every
check and heartbeat on it. Without `status:read` it is not in
`status.json`, the overall status ignores it, `components/{id}` and its
feed are 404, `timeline?component=` refuses it like an unknown id, and
`checks`, `events`, `outages`, the MCP tools, `/llms.txt`, the status
page and the subscribe page leave it and its checks out. A group whose
components are all non-public does not appear. With `status:read`, and
on the internal board, it is listed under its group like any other. An
incident or maintenance may not name a non-public component (422); a
`[[project]]` may list one, since releases carry no component ids.

### Draft incidents

A [draft incident](incidents.md#drafts) is internal the same way until a
person publishes it: the internal board and a `status:read` token see it,
and no read without that scope names it. `incidents` and `timeline` leave
it out, `incidents/{id}` is 404 for it, and `status.json`,
`components/{id}`, `/llms.txt`, the feeds and the status page never list
it or let it move a component's health.

An incident's [internal note and external key](incidents.md#internal-note-and-external-key)
are internal for good, draft or published: `incidents` and
`incidents/{id}` include them only for a `status:read` token, no other
read ever names them, and `incidents?external_key=` is 403 without that
scope.

Likewise a `[[project]]` can be non-public. A non-public project is left
out wholesale, not redacted: it is not in the release list, its deploys
are not on the timeline, and `/llms.txt` does not mention it.

```toml
[[token]]
id = "dashboards"
secret = "${ENV:TOCSIN_READ_TOKEN}"
scopes = ["status:read"]
description = "Grafana and the agents that read history"
```

The full `[[token]]` reference is in [`config.md`](config.md#token).

## Endpoints

| Method and path | Token | Success |
|---|---|---|
| `GET /api/v1/status.json` | none | 200, `{title, status, generated_at, components, incidents}` |
| `GET /api/v1/components/{id}` | none, or `status:read` | 200, `{component, checks, incidents}` |
| `GET /api/v1/checks` | none, or `status:read` | 200, `{checks}` |
| `GET /api/v1/checks/{id}/results?since=&before=&limit=` | `status:read` | 200, `{check_id, since, results, next_before}`, newest first |
| `GET /api/v1/outages?since=&before=&limit=` | none, or `status:read` | 200, `{since, outages, next_before}`, newest start first |
| `GET /api/v1/alerts/history?since=&before=&limit=&state=` | `status:read` | 200, `{since, alerts, next_before}`, newest start first |
| `GET /api/v1/incidents?limit=&before=` | none; `status:read` adds drafts and internals | 200, `{incidents, next_before}`, newest first |
| `GET /api/v1/incidents?external_key=` | `status:read` | 200, `{incidents, next_before}`, the incidents with that key, newest first, paged by `limit` and `before` |
| `GET /api/v1/incidents/{id}` | none; `status:read` for a draft and internals | 200, the incident |
| `GET /api/v1/releases?project=&since=&before=&limit=` | none, or `status:read` | 200, `{releases, truncated, next_before}`, newest first |
| `GET /api/v1/timeline?component=&since=&after=&limit=` | none, or `status:read` | 200, `{since, events, truncated, next_after}`, oldest first |
| `GET /api/v1/events` | none, or `status:read` | 200, an endless `text/event-stream` |
| `GET /api/v1/config/status` | `status:read` | 200, `{loaded_at, last_attempt_at, ok, errors}`; see [config.md](config.md#reloading) |
| `GET /api/v1/openapi.json` | none | 200, the OpenAPI document |
| `GET /llms.txt` | none | 200, `text/plain` |
| `GET /metrics` | `status:read` | 200, Prometheus text format 0.0.4; not in the OpenAPI document, see [metrics.md](metrics.md) |
| `GET /api/v1/silences?before=&limit=` | `silence` | 200, `{silences, next_before}`, newest first; see [alerting.md](alerting.md#silences) |
| `GET /api/v1/silences/{id}` | `silence` | 200, the silence with the messages it held back |
| `POST /api/v1/silences` | `silence` | 201, the silence; see [alerting.md](alerting.md#silences) |
| `POST /api/v1/silences/{id}/expire` | `silence` | 200, the silence, ended |
| `POST /api/v1/subscribers` | none | 202, the same for every address; 429 past a limit. Only with `[subscribers]`; see [subscribers.md](subscribers.md#signing-up) |
| `GET /api/v1/subscribers?after=&limit=` | `subscribers:admin` | 200, `{subscribers, next_after, totals}`; see [subscribers.md](subscribers.md#for-operators) |
| `DELETE /api/v1/subscribers/{id}` | `subscribers:admin` | 204 |

### Status now

- **`status.json`** is the page as JSON. `status` is the worst component
  status, ranked operational, unknown, maintenance, degraded, down; with no
  components it is `unknown`. Each component carries its `status` (what
  the page shows: its checks' health, overridden by incidents and
  maintenance) and `health` (its checks alone, `null` until one reports).
  `incidents` are the open incidents and maintenance.
- **`components/{id}`** is one component with every check and heartbeat
  that sets its health and the open incidents naming it. An unknown id is
  404, and so is a component with `public = false` without `status:read`.
- **`checks`** lists every check and heartbeat on a component, and with
  a `status:read` token those on none too. `kind` is `check` or
  `heartbeat`, so heartbeats sit beside checks rather than in a second
  list; `type` is the probe (`http`, `tcp`, `dns`, `icmp`, `tls`) and
  `null` for a heartbeat. `health`, `since`, `last_health` and
  `last_result_at` are the engine's live state on this node, for a
  heartbeat as for a check: its last ping or missed deadline, and the
  state they confirmed. Checks and heartbeats share one id space; the
  configuration loader refuses a check and a heartbeat with the same id.

### History

- **`checks/{id}/results`** is one check's or heartbeat's raw results,
  newest first: `started_at`, `duration_ms`, `health`, `error`,
  `status_code`, the probe's `detail` and the `node_id` that ran it. A
  heartbeat's results are its pings (health from the ping's status, its
  message in `detail`) and the `down` results its missed deadlines wrote.
  An id no check or heartbeat has is 404, even one with old results still
  stored.
- **`outages`** lists every outage open at any moment in the window: one
  that started before `since` but ended after it is included, as is one
  still open (`ended_at: null`). `subject_kind` says whether `check_id`
  is a check or a heartbeat, as on the timeline's outage events. Without
  `status:read` only outages of checks on a component are listed, and a
  `before` naming any other outage is 422, as an unknown one is.
  `ending` says how an outage ended, and is `null` while it is open:
  - `recovered`: tocsin saw the check leave this outage's health. That is
    a return to up, or a move between degraded and down, which ends one
    outage and opens the next at the new health.
  - `withdrawn`: the check or heartbeat was removed from the configuration
    while out, so tocsin stopped watching it at that reload (see
    [config.md](config.md#reloading)). Its `ended_at` is when tocsin
    stopped watching, not a recovery; nobody saw it come back.

  Outages that ended before `ending` existed (schema 8) read `recovered`.
  `node_id` is public here, as it is on the timeline: it is the
  `[server] node_id` setting, which defaults to the host name, so set it
  in config if the host name is sensitive.
- **`alerts/history`** lists every alert open at any moment in the
  window, one per notifier per check or heartbeat per outage: one that
  started before `since` but ended after it is included, as is one still
  firing. `state=firing` or `state=resolved` keeps to one kind; keep it
  the same while paging. `labels` are exactly the labels every
  Alertmanager message of the alert carried, fire, repeat and resolve
  alike, built by the same code that posted them; `ended_by` is
  `recovery`, or `config` when a reload removed the check, heartbeat or
  notifier while it fired. `deliveries` lists each fire, repeat, resolve
  and cancel queued for it, oldest first: `status` is `pending` (with
  `attempts` and `last_error` while it is retrying), `delivered`,
  `failed` (given up on, with its last error) or `silenced` (with the
  `silence_id` that held it). Retention bounds it: delivered rows go
  after a week and failed and silenced ones after 90 days, and an ended
  alert goes with its last delivery (see
  [alerting.md](alerting.md#delivery)). So a month back shows every
  failure but only the past week's successes. A firing alert is never
  pruned, but its early deliveries can be, so `notified` says whether
  its notifier accepted a fire or repeat for its current health, and
  `attempted` whether one was ever sent at all. A cursor only places
  the page: one whose alert resolved since still pages `state=firing`
  on.
- **`timeline`** merges outages, incidents, deploys and releases; see
  [releases.md](releases.md#http-api). `component=` narrows it to one
  component's checks, incidents and projects. Without `status:read` its
  outages are only those of checks on a public component, and a
  non-public `component=` is a 422 like an unknown id.

### Windows and paging

| Parameter | Meaning |
|---|---|
| `since` | RFC 3339. The start of the window: at most 366 days ago. Defaults to one day ago for check results and 30 days ago for outages, alerts and the timeline. (Releases take it as a plain filter with no default.) |
| `limit` | 1 to 500, default 100 (incidents: 1 to 100, default 20; releases: 1 to 200, default 50). |
| `before` | The previous page's `next_before`. Keep `since` the same. |
| `after` | The timeline's previous `next_after`. Keep `since` the same. |

`next_before` (or `next_after`) is set when more follows and `null` on the
last page, even a full one. Unknown query parameters are refused.

A `before` that names no row of that read is 422 with the field path
`before`: the row was purged by retention since the previous page, or the
cursor came from another check's results. Start again without `before`.
It is never an empty page, which would look like the end.

### Errors

Errors are the same JSON every endpoint returns; the full table is in
[incidents.md](incidents.md#errors).

```json
{
  "error": "invalid",
  "message": "limit: must be 1 to 500",
  "fields": [{ "path": "limit", "message": "must be 1 to 500" }]
}
```

| Status | `error` | When |
|---|---|---|
| 401 | `unauthorized` | A malformed header or unknown token on any read, or no token on check results or the configuration status. Carries `WWW-Authenticate: Bearer`. |
| 403 | `forbidden` | Check results or the configuration status with a token that lacks `status:read`. |
| 404 | `not_found` | No component, check or incident with that id. |
| 422 | `invalid` | A query value breaks a rule. `fields[].path` is the parameter (`since`, `limit`); `""` when the query does not parse at all (`limit=x`, an unknown parameter). |

### Events

`GET /api/v1/events` streams every change from the moment you connect, as
[server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html).
The event name is the change's `type` (`incident_created`,
`incident_updated`, `incident_edited`, `incident_published`,
`incident_discarded`, `release_published`,
`release_deployed`, `check_changed`, `component_changed`) and the data is
its JSON, as described in [incidents.md](incidents.md#events),
[releases.md](releases.md#events) and [engine.md](engine.md#events).

The first event on every connection is `resync`, with data `{}`, and so
is the one after a slow reader fell behind and missed some: on either,
re-read whatever you show rather than patching it. A comment line every 15
seconds keeps a quiet connection open through proxies, and the response
carries `X-Accel-Buffering: no` so nginx passes events through at once.
The stream ends when the server shuts down; reconnect.

Anyone sees incident events, changes of public components and of the
checks on them, and the releases of public projects; a `status:read`
token also sees non-public components, changes of checks on no
component, releases of private ones, and events about draft incidents.
A stream without `status:read` also ends when the configuration reloads,
since a component may have changed sides; reconnect. The status page's live updates are this stream; the internal board
has its own, `/internal/events`.

## `/llms.txt`

A plain-text page for agents, in the [llms.txt](https://llmstxt.org)
layout: the page title, the overall status, a link per component with its
status, the open incidents, then links to the OpenAPI document, this
reference and each history endpoint with an example `since`. It shows only
what `status.json` shows the public. Links are absolute when
`[server] public_url` is set, and relative otherwise.

## CLI

The query commands are HTTP clients of a running server. `--url` or
`TOCSIN_URL` names it; `--token` or `TOCSIN_TOKEN` is sent with every read
when set. A token without `status:read` reads the public view, so a write
token in the environment is harmless; `history` and `alerts` need `status:read`
and exit 2 on the 401 or 403 without it, with a `hint` (in `--json`, under
`error`) naming the `status:read` scope it needs. Leave it unset to read as
the public.

```sh
export TOCSIN_URL=https://status.example.com TOCSIN_TOKEN=...

tocsin status [--json]
tocsin history <check> [--since 2h] [--before ID] [--limit N] [--json]
tocsin outages [--since 7d] [--before ID] [--limit N] [--json]
tocsin alerts [--state firing|resolved] [--since 7d] [--before ID] [--limit N] [--json]
tocsin timeline [--component api] [--since 1d] [--after CURSOR] [--limit N] [--json]
```

`--since` takes a duration back from now (`90s`, `2h`, `7d`, `1h30m`) or an
RFC 3339 instant. Without it the server's default window applies. Text
output ends with `more: --before N` (or `more: --after C`) when another
page follows.

With `--json`, stdout is exactly one JSON document: the endpoint's
response as it came, field for field, or on failure `{"error": {kind,
message, errors}}`. A newer server's extra fields and enum values pass
through untouched; the text output prints an unknown value as it is.

| Exit | When |
|---|---|
| 0 | The read succeeded (and for `status`, nothing is degraded or down). |
| 1 | `tocsin status` only: the overall status is `degraded` or `down`. |
| 2 | Invalid input: a bad `--since`, a check id that is not a valid id, or any 4xx (401, 403, 404, 422). |
| 3 | The server cannot be reached, answers 5xx, or answers 2xx with a body that is not the expected JSON. |

`history` checks the id before sending it, since it goes into the request
path.
