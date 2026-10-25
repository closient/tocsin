# Incidents and maintenance

An incident is something going wrong that people declare and narrate; a
maintenance is planned work announced ahead of time. Both are written
through the HTTP API, by hand with `tocsin incident` and
`tocsin maintenance`, or by any tool that holds a token. Monitors never open
incidents on their own.

An incident can start as a [draft](#drafts): visible to operators only, and
told to nobody, until a person publishes it.

## The model

| Field | |
|---|---|
| `id` | Assigned by tocsin, never reused. |
| `kind` | `incident` or `maintenance`. Fixed at creation. |
| `title` | One line, shown on the status page: no line breaks, tabs or other control characters. |
| `impact` | `none`, `minor`, `major` or `critical`. Defaults to `none`. |
| `components` | Ids of `[[component]]` entries it affects, sorted. An id the configuration does not define, or one of a component with `public = false`, is refused. |
| `status` | The status of the latest update. |
| `scheduled_start`, `scheduled_end` | Maintenance only, and required for it; the end is after the start. |
| `resolved_at` | When it first reached `resolved` (incident) or `completed` (maintenance); cleared if an update reopens it. |
| `created_at`, `created_by` | When and by whom it was opened. |
| `published_at`, `published_by` | When and by whom it went public; both `null` while it is a [draft](#drafts). An incident created published has the creation's time and author. |
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
for a write made with a `[[token]]`, and `{"kind": "person", "id": "<email>"}`
for one made on the [admin page](admin.md), as the authenticating proxy
named them. Timestamps are UTC, RFC 3339
with milliseconds, such as `2026-11-01T02:00:00.000Z`; the API accepts any
RFC 3339 offset and stores UTC.

How the tables store this, and how edits are audited, is in
[`storage.md`](storage.md#incident-incident_component-incident_update-and-incident_edit).

## Drafts

A draft is an incident nobody outside has seen. An agent or a person writes
it, the people who run the service read it on the
[internal board](status-page.md) and the [admin page](admin.md), and one of
them publishes it when it says the right thing. Until then:

- It is **not public**. The status page, its fragments and incident pages,
  `status.json`, the incidents and timeline API without a `status:read`
  token, the Atom feeds, the event stream, `llms.txt` and the Markdown
  export all leave it out; `GET /api/v1/incidents/{id}` and
  `/incidents/{id}` answer 404 for it. A `status:read` token sees it, with
  `published_at: null`.
- It **tells nobody**. Creating it, updating it or editing it sends no
  email and no Slack post, whatever `notify` says.
- It **is a real incident otherwise**: updates append to its timeline,
  edits change it, and its status moves, so the draft a person publishes
  is already the whole story.
- It does not change any component's status, so the page's banner and
  the engine's notices ignore it.

**Publishing** stamps `published_at` and `published_by`, emits
`incident_published`, and tells subscribers once, with the newest update's
message, exactly as a new incident would. From then on it is an ordinary
incident. Publishing twice is 409. `"notify": false` publishes it without
telling anyone.

**Discarding** deletes a draft and its timeline. A published incident cannot
be discarded (409); resolve it instead.

| Where | Create as a draft | Publish | Discard |
|---|---|---|---|
| HTTP | `POST /api/v1/incidents` with `"draft": true` | `POST /api/v1/incidents/{id}/publish` | `DELETE /api/v1/incidents/{id}` |
| CLI | `tocsin incident new --draft` | `tocsin incident publish <id>` | `tocsin incident discard <id>` |
| MCP | `create_incident` (drafts by default) | none, on purpose | `discard_incident` |
| Admin | "Save as draft" on the new-incident form | Publish on the incident's page | Discard on the incident's page |
| Internal board | | Publish beside each draft | Discard beside each draft |

Drafts are listed at the top of the internal board and marked on the admin
page with who wrote them.

## Tokens

Writes need a bearer token whose `[[token]]` entry has one of two scopes:

| Scope | May |
|---|---|
| `incident` | Everything: create published or as a draft, update, edit, publish and discard. |
| `incident:draft` | Create drafts, and update, edit and discard drafts. Publishing, creating with `"draft": false`, and any write to a published incident are 403. |

Give an agent `incident:draft`, so what it writes waits for a person. An
`incident:draft` token's incident is a draft when `draft` is left out; an
`incident` token's is published, except over MCP, which drafts unless
`draft: false` is passed.

```toml
[[token]]
id = "oncall"                      # recorded as the author of every write
secret = "${ENV:TOCSIN_ONCALL_TOKEN}"  # an environment reference, never the value
scopes = ["incident"]
description = "The on-call rota"
```

The token's `id`, never its secret, is what the API records and logs. Every
configured secret is compared in constant time, so a response does not
reveal how much of a guess was right. Reads need no token ([api.md](api.md) has the whole read API). The full
`[[token]]` reference is in [`config.md`](config.md#token).

## HTTP API

`tocsin serve` listens on `[server] listen`. The OpenAPI 3.1 document is
served at `GET /api/v1/openapi.json` and committed as
[`schema/openapi.json`](../schema/openapi.json).

| Method and path | Token | Body | Success |
|---|---|---|---|
| `GET /api/v1/incidents?limit=&before=` | none; `status:read` adds drafts | | 200, `{incidents, next_before}` |
| `GET /api/v1/incidents/{id}` | none; `status:read` for a draft | | 200, the incident |
| `POST /api/v1/incidents` | `incident` or `incident:draft` | `{kind, title, impact, components, status, message, scheduled_start, scheduled_end, notify, draft}` | 201, the incident, with `Location` |
| `POST /api/v1/incidents/{id}/updates` | `incident`, or `incident:draft` on a draft | `{status, message, notify}` | 201, the incident |
| `PATCH /api/v1/incidents/{id}` | `incident`, or `incident:draft` on a draft | any of `{title, impact, components, scheduled_start, scheduled_end}` | 200, the incident |
| `POST /api/v1/incidents/{id}/publish` | `incident` | `{notify}`, or no body | 200, the incident |
| `DELETE /api/v1/incidents/{id}` | `incident` or `incident:draft` | none | 200, the discarded draft |

- **List** returns newest first. `limit` is 1 to 100 (default 20).
  `next_before` is set when more incidents follow; pass it as `before` for
  the next page. It is `null` on the last page, even a full one.
- **Create** needs `title` and `message`; `kind` defaults to `incident`,
  `impact` to `none`, `components` to none, and `status` to the kind's first
  status. Maintenance needs `scheduled_start` and `scheduled_end`.
  `"notify": false` (default `true`) opens it without emailing subscribers.
  `"draft": true` makes it a [draft](#drafts); left out, it follows the
  token's scope.
- **Update** appends to the timeline and moves `status`. The status must
  belong to the incident's kind. With [email subscribers](subscribers.md)
  on, each new incident and update is emailed to the subscribers of its
  components; `"notify": false` (default `true`) posts an update without
  emailing it, for a typo fix or a minor note. An update to a draft is
  never sent.
- **Publish** and **discard** act on a draft only; on a published incident
  they are 409.
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
| 403 | `forbidden` | The token lacks the scope the write needs; an `incident:draft` token publishing or writing to a published incident. The message names the token id. |
| 404 | `not_found` | No incident with that id (or a draft, to a caller without `status:read`), or no such route. |
| 409 | `conflict` | Publishing an incident already published, or discarding one. |
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
update was appended), `incident_edited`, `incident_published` or
`incident_discarded`, each carrying the incident as the API returns it.
Refused writes emit nothing. An event about a draft reaches only the
internal board's stream and `status:read` readers; the public stream first
hears of a draft when it is published. The engine publishes
`check_changed` and `component_changed` on the same bus, and rereads
incidents on every incident event, so a posted incident moves its
components at once; see [engine.md](engine.md#events).

## CLI

The incident commands are HTTP clients of a running `tocsin serve`, local or
remote.

| Setting | Flag | Environment |
|---|---|---|
| Server base URL | `--url` | `TOCSIN_URL` |
| Bearer token (writes, and reading drafts) | `--token` | `TOCSIN_TOKEN` |

The flag wins over the variable. Prefer the variable for the token, so it
stays out of shell history and process listings.

```sh
export TOCSIN_URL=https://status.example.com TOCSIN_TOKEN=...

tocsin incident new --title "API errors" --impact major \
  --component api --component web --message "Looking into elevated errors."
tocsin incident new --title "Typo test" --message "Ignore." --quiet  # no email
tocsin incident new --title "API errors" --message "Looking into it." --draft
tocsin incident publish 9 [--quiet]
tocsin incident discard 10
tocsin incident update 7 --status identified --message "A bad deploy; rolling back."
tocsin incident update 7 --status identified --message "Fixed a typo." --quiet  # no email
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
`update`, `edit`, `show`, `publish`, `discard` and `schedule`; `{incidents, next_before}` for
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
`resolved_at` once it is over. The export reads without a token, so a draft
is never exported: `--all` skips it and `export <id>` is a 404. Authors are written `kind:id`. Edits do not
appear in the export; the `incident_edit` table keeps them.
