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
| Outages and incidents | yes | yes |
| Releases and deploys of a project with `public = true` | yes | yes |
| Releases and deploys of a project with `public = false` | no | yes |

On the endpoints that show a reader more (components, checks, check
results, releases and the timeline), an `Authorization` header is always
checked: an unknown token is 401 and a token without `status:read` (an
`incident` or `release` token, say) is 403, even though the public may
read them. Leave the header off to read as the public. `status.json`,
outages, incidents and `/llms.txt` are the same for everyone and ignore
the header. A non-public project is left
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
| `GET /api/v1/outages?since=&before=&limit=` | none | 200, `{since, outages, next_before}`, newest start first |
| `GET /api/v1/incidents?limit=&before=` | none | 200, `{incidents, next_before}`, newest first |
| `GET /api/v1/incidents/{id}` | none | 200, the incident |
| `GET /api/v1/releases?project=&since=&before=&limit=` | none, or `status:read` | 200, `{releases, truncated, next_before}`, newest first |
| `GET /api/v1/timeline?component=&since=&after=&limit=` | none, or `status:read` | 200, `{since, events, truncated, next_after}`, oldest first |
| `GET /api/v1/openapi.json` | none | 200, the OpenAPI document |
| `GET /llms.txt` | none | 200, `text/plain` |

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
  `last_result_at` are the engine's live state on this node.

### History

- **`checks/{id}/results`** is one check's or heartbeat's raw results,
  newest first: `started_at`, `duration_ms`, `health`, `error`,
  `status_code`, the probe's `detail` and the `node_id` that ran it. An id
  no check or heartbeat has is 404, even one with old results still
  stored.
- **`outages`** lists every outage open at any moment in the window: one
  that started before `since` but ended after it is included, as is one
  still open (`ended_at: null`). `kind` says whether `check_id` is a
  check or a heartbeat.
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
| 401 | `unauthorized` | An unknown token, or none where one is required. Carries `WWW-Authenticate: Bearer`. |
| 403 | `forbidden` | The token lacks `status:read`. |
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
when set, so for `history` and `timeline` it must be a `status:read`
token (an `incident` or `release` token gets 403). Leave it unset to read
as the public.

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
response as it came, or on failure `{"error": {kind, message, errors}}`.

| Exit | When |
|---|---|
| 0 | The read succeeded (and for `status`, nothing is degraded or down). |
| 1 | `tocsin status` only: the overall status is `degraded` or `down`. |
| 2 | Invalid input: a bad `--since`, a check id that is not a valid id, or any 4xx (401, 403, 404, 422). |
| 3 | The server cannot be reached or answers 5xx. |

`history` checks the id before sending it, since it goes into the request
path.
