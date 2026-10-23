# Incidents and maintenance

An incident is something going wrong that people declare and narrate; a
maintenance is planned work announced ahead of time. Both are written
through the HTTP API, by hand with `tocsin incident` and
`tocsin maintenance`, or by any tool that holds a token. Monitors never open
incidents on their own.

## The model

| Field | |
|---|---|
| `id` | Assigned by tocsin, never reused. |
| `kind` | `incident` or `maintenance`. Fixed at creation. |
| `title` | One line, shown on the status page: no line breaks, tabs or other control characters. |
| `impact` | `none`, `minor`, `major` or `critical`. Defaults to `none`. |
| `components` | Ids of `[[component]]` entries it affects, sorted. An id the configuration does not define is refused. |
| `status` | The status of the latest update. |
| `scheduled_start`, `scheduled_end` | Maintenance only, and required for it; the end is after the start. |
| `resolved_at` | When it first reached `resolved` (incident) or `completed` (maintenance); cleared if an update reopens it. |
| `created_at`, `created_by` | When and by whom it was opened. |
| `updated_at`, `updated_by` | The last update or edit, and who sent it. |
| `updates` | The timeline, oldest first. |

Each update has a `status`, a Markdown `message`, `created_at` and
`created_by`. Updates are append-only; the first is the opening message.

Statuses depend on the kind:

- **incident**: `investigating` (the default first status), `identified`,
  `monitoring`, `resolved`.
- **maintenance**: `scheduled` (the default first status), `in_progress`,
  `completed`.

An author (`created_by`, `updated_by`) is `{"kind": "token", "id": "<token id>"}`
for a write made with a `[[token]]`. The kind `person` is reserved for
identities an authenticating proxy will supply. Timestamps are UTC, RFC 3339
with milliseconds, such as `2026-11-01T02:00:00.000Z`; the API accepts any
RFC 3339 offset and stores UTC.

How the tables store this, and how edits are audited, is in
[`storage.md`](storage.md#incident-incident_component-incident_update-and-incident_edit).

## Tokens

Writes need a bearer token whose `[[token]]` entry has the `incident` scope:

```toml
[[token]]
id = "oncall"                      # recorded as the author of every write
secret = "${ENV:TOCSIN_ONCALL_TOKEN}"  # an environment reference, never the value
scopes = ["incident"]
description = "The on-call rota"
```

The token's `id`, never its secret, is what the API records and logs. Every
configured secret is compared in constant time, so a response does not
reveal how much of a guess was right. Reads need no token. The full
`[[token]]` reference is in [`config.md`](config.md#token).

## HTTP API

`tocsin serve` listens on `[server] listen`. The OpenAPI 3.1 document is
served at `GET /api/v1/openapi.json` and committed as
[`schema/openapi.json`](../schema/openapi.json).

| Method and path | Token | Body | Success |
|---|---|---|---|
| `GET /api/v1/incidents?limit=&before=` | none | | 200, `{incidents, next_before}` |
| `GET /api/v1/incidents/{id}` | none | | 200, the incident |
| `POST /api/v1/incidents` | `incident` | `{kind, title, impact, components, status, message, scheduled_start, scheduled_end}` | 201, the incident, with `Location` |
| `POST /api/v1/incidents/{id}/updates` | `incident` | `{status, message}` | 201, the incident |
| `PATCH /api/v1/incidents/{id}` | `incident` | any of `{title, impact, components, scheduled_start, scheduled_end}` | 200, the incident |

- **List** returns newest first. `limit` is 1 to 100 (default 20).
  `next_before` is set when more incidents follow; pass it as `before` for
  the next page. It is `null` on the last page, even a full one.
- **Create** needs `title` and `message`; `kind` defaults to `incident`,
  `impact` to `none`, `components` to none, and `status` to the kind's first
  status. Maintenance needs `scheduled_start` and `scheduled_end`.
- **Update** appends to the timeline and moves `status`. The status must
  belong to the incident's kind.
- **Edit** changes only the fields it names; `components` replaces the whole
  set (`[]` clears it). It adds nothing to the timeline. A schedule on an
  incident, or one that ends before it starts, is refused.

Send JSON with `Content-Type: application/json`. Unknown fields are refused.

### Errors

Every error, including an unknown route (404) or method (405), is JSON:

```json
{
  "error": "invalid",
  "message": "components[1]: no [[component]] has id \"billing\"",
  "fields": [{ "path": "components[1]", "message": "no [[component]] has id \"billing\"" }]
}
```

| Status | `error` | When |
|---|---|---|
| 400 | `bad_request` | The body is not JSON, or has trailing data. |
| 401 | `unauthorized` | No bearer token, or one no `[[token]]` has. Carries `WWW-Authenticate: Bearer`. |
| 403 | `forbidden` | The token lacks the `incident` scope. The message names the token id. |
| 404 | `not_found` | No incident with that id, or no such route. |
| 405 | `method_not_allowed` | The route exists; the method does not. |
| 413 | `too_large` | The body is over 2 MiB (2,097,152 bytes). |
| 415 | `bad_request` | The body is not sent as JSON. |
| 422 | `invalid` | The body or query breaks a rule. `fields` lists each problem at its path (`title`, `components[1]`, `scheduled_end`); a path of `""` is the request as a whole. |
| 500 | `internal` | Logged on the server; the response says only that. |

The token is checked before the body is read, so a request without a valid
token learns nothing about the body rules.

### Events

Every successful write emits one event on the server's internal event bus,
after its transaction commits: `incident_created`, `incident_updated` (an
update was appended) or `incident_edited`, each carrying the incident as the
API returns it. Refused writes emit nothing. The engine publishes
`check_changed` and `component_changed` on the same bus, and rereads
incidents on every incident event, so a posted incident moves its
components at once; see [engine.md](engine.md#events).

## CLI

The incident commands are HTTP clients of a running `tocsin serve`, local or
remote.

| Setting | Flag | Environment |
|---|---|---|
| Server base URL | `--url` | `TOCSIN_URL` |
| Bearer token (writes only) | `--token` | `TOCSIN_TOKEN` |

The flag wins over the variable. Prefer the variable for the token, so it
stays out of shell history and process listings.

```sh
export TOCSIN_URL=https://status.example.com TOCSIN_TOKEN=...

tocsin incident new --title "API errors" --impact major \
  --component api --component web --message "Looking into elevated errors."
tocsin incident update 7 --status identified --message "A bad deploy; rolling back."
tocsin incident edit 7 --title "API errors in eu-west" --component api
tocsin incident edit 7 --no-components
tocsin incident show 7
tocsin incident list --json [--limit 50] [--before 7]

tocsin maintenance schedule --start 2026-11-01T02:00:00Z --end 2026-11-01T03:00:00Z \
  --component api --message "Upgrading the database." [--title "..."] [--impact minor]
tocsin incident update 8 --status in_progress --message "Started."

tocsin incident export 7 --format md               # print one
tocsin incident export 7 --format md --out notes/  # write notes/incident-7.md
tocsin incident export --all --format md --out notes/
```

With `--json`, stdout is exactly one JSON document: the incident for `new`,
`update`, `edit`, `show` and `schedule`; `{incidents, next_before}` for
`list`; `{id, format, markdown}` for a printed export; and
`{format, files: [{id, path}]}` when `--out` writes files. Logs go to stderr.

Exit codes follow the CLI contract: 0 on success; 2 for invalid input,
including any 4xx response (no token, 401, 403, 404, 422) and a write with
no token configured; 3 for a runtime error, including a server that cannot
be reached and any 5xx. A 422's field paths appear in the error as
diagnostics.

## Export format

`incident export --format md` writes Markdown with YAML front matter, one
file per incident. Each front-matter value is JSON, which is valid YAML, so
titles with colons or quotes need no escaping:

```markdown
---
id: 7
kind: "incident"
title: "API errors in eu-west"
impact: "major"
status: "resolved"
components: ["api"]
resolved_at: "2026-10-21T10:40:00.000Z"
created_at: "2026-10-21T10:02:00.000Z"
created_by: "token:oncall"
updated_at: "2026-10-21T10:45:00.000Z"
updated_by: "token:oncall"
---

# API errors in eu-west

## 2026-10-21T10:02:00.000Z — investigating

_by token:oncall_

Looking into elevated errors.

## 2026-10-21T10:40:00.000Z — resolved

_by token:oncall_

Fixed.
```

`scheduled_start` and `scheduled_end` appear for maintenance, and
`resolved_at` once it is over. Authors are written `kind:id`. Edits do not
appear in the export; the `incident_edit` table keeps them.
