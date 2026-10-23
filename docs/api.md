# Read API

What a running `tocsin serve` says about itself: the status now, each
component and check, raw check results, outages, incidents, releases and
the timeline. Writes are documented with their models in
[incidents.md](incidents.md#http-api) and [releases.md](releases.md#http-api).

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
| Checks and heartbeats: id, kind, name, interval, health | yes | yes |
| A check's `target` (its URL or host, which can name an internal host) | no | yes |
| A check's raw results (`/checks/{id}/results`) | no, 401 | yes |
| How the configuration last reloaded (`/config/status`), whose errors name files and environment variables | no, 401 | yes |
| Prometheus metrics (`/metrics`, see [metrics.md](metrics.md)) | no, 401 | yes |
| Outages and incidents | yes | yes |
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

Only a `[[project]]` can be non-public; components, checks and their
outages have no visibility setting and are on the public page. A
non-public project is left out wholesale, not redacted: it is not in the
release list, its deploys are not on the timeline, and `/llms.txt` does
not mention it.

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
| `GET /api/v1/outages?since=&before=&limit=` | none | 200, `{since, outages, next_before}`, newest start first |
| `GET /api/v1/incidents?limit=&before=` | none | 200, `{incidents, next_before}`, newest first |
| `GET /api/v1/incidents/{id}` | none | 200, the incident |
| `GET /api/v1/releases?project=&since=&before=&limit=` | none, or `status:read` | 200, `{releases, truncated, next_before}`, newest first |
| `GET /api/v1/timeline?component=&since=&after=&limit=` | none, or `status:read` | 200, `{since, events, truncated, next_after}`, oldest first |
| `GET /api/v1/config/status` | `status:read` | 200, `{loaded_at, last_attempt_at, ok, errors}`; see [config.md](config.md#reloading) |
| `GET /api/v1/openapi.json` | none | 200, the OpenAPI document |
| `GET /llms.txt` | none | 200, `text/plain` |
| `GET /metrics` | `status:read` | 200, Prometheus text format 0.0.4; not in the OpenAPI document, see [metrics.md](metrics.md) |

### Status now

- **`status.json`** is the page as JSON. `status` is the worst component
  status, ranked operational, unknown, maintenance, degraded, down; with no
  components it is `unknown`. Each component carries its `status` (what
  the page shows: its checks' health, overridden by incidents and
  maintenance) and `health` (its checks alone, `null` until one reports).
  `incidents` are the open incidents and maintenance.
- **`components/{id}`** is one component with every check and heartbeat
  that sets its health and the open incidents naming it. An unknown id is
  404.
- **`checks`** lists every check and heartbeat. `kind` is `check` or
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
  is a check or a heartbeat, as on the timeline's outage events.
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
- **`timeline`** merges outages, incidents, deploys and releases; see
  [releases.md](releases.md#http-api). `component=` narrows it to one
  component's checks, incidents and projects.

### Windows and paging

| Parameter | Meaning |
|---|---|
| `since` | RFC 3339. The start of the window: at most 366 days ago. Defaults to one day ago for check results and 30 days ago for outages and the timeline. (Releases take it as a plain filter with no default.) |
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
token in the environment is harmless; `history` alone needs `status:read`
and exits 2 on the 401 or 403 without it, with a `hint` (in `--json`, under
`error`) naming the `status:read` scope it needs. Leave it unset to read as
the public.

```sh
export TOCSIN_URL=https://status.example.com TOCSIN_TOKEN=...

tocsin status [--json]
tocsin history <check> [--since 2h] [--before ID] [--limit N] [--json]
tocsin outages [--since 7d] [--before ID] [--limit N] [--json]
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
