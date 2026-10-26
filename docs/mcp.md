# MCP server

tocsin speaks the [Model Context Protocol](https://modelcontextprotocol.io),
so an agent can read the status, outages and check history, and post
incidents and releases, as tools rather than shell commands. There are two
ways in, with the same tools:

- **`/mcp` on a running server**: streamable HTTP, behind the same bearer
  tokens as the [HTTP API](api.md). Nothing to install on the agent's
  machine.
- **`tocsin mcp`**: the tools over stdin and stdout, calling a server's
  HTTP API. It also offers `validate_config`, which checks a configuration
  folder on the agent's own machine.

Every tool is the API route it names, run by the same code: the answers,
errors and limits are the REST ones. Writes are recorded as the token's
`[[token]]` id, as they are from the CLI.

## Setting it up

Give the agent a token with the scopes it needs (see [Tools](#tools)).
`incident:draft` lets it draft, edit and discard incidents but never
publish one, so a person reviews what it wrote before anyone is told
(see [Drafts](incidents.md#drafts)):

```toml
[[token]]
id = "agent"
secret = "${ENV:TOCSIN_AGENT_TOKEN}"
scopes = ["status:read", "incident:draft"]
description = "Claude Code on the on-call laptop"
```

### Claude Code

Over HTTP:

```sh
claude mcp add --transport http tocsin https://status.example.com/mcp \
  --header "Authorization: Bearer $TOCSIN_TOKEN"
```

Over stdio, with `tocsin` on the agent's `PATH`:

```sh
claude mcp add tocsin \
  --env TOCSIN_URL=https://status.example.com \
  --env TOCSIN_TOKEN="$TOCSIN_TOKEN" \
  -- tocsin mcp
```

### Other clients

Most clients read a JSON file of servers. Over HTTP:

```json
{
  "mcpServers": {
    "tocsin": {
      "type": "http",
      "url": "https://status.example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Over stdio:

```json
{
  "mcpServers": {
    "tocsin": {
      "command": "tocsin",
      "args": ["mcp"],
      "env": {
        "TOCSIN_URL": "https://status.example.com",
        "TOCSIN_TOKEN": "<token>"
      }
    }
  }
}
```

`tocsin mcp` takes `--url` and `--token` too, but arguments show in `ps`;
prefer the environment. Its stdout carries the protocol and nothing else,
logs go to stderr, and it runs until the client closes stdin. It exits 2
when the URL is not an `http` or `https` URL, and 3 when the handshake
fails.

To try it by hand, the MCP Inspector lists and calls the tools:

```sh
npx @modelcontextprotocol/inspector --cli https://status.example.com/mcp \
  --transport http --header "Authorization: Bearer $TOCSIN_TOKEN" \
  --method tools/list
```

## Who may call what

On `/mcp`, every request needs `Authorization: Bearer <token>` naming a
`[[token]]`. Without one, or with one tocsin does not know, the answer is
HTTP 401 and no MCP exchange happens, reads included. The scope a tool
needs is checked on each call, exactly as its route checks it: a tool
the token may not use is a tool error naming the missing scope, and
nothing is written.

`tocsin mcp` sends its token, if it has one, to the HTTP API, so the API's
rules apply as they are: without a token the read tools see the public
view, and a tool that needs a scope fails with "this needs a token"
before it sends anything.

## Tools

An agent's incident starts as a [draft](incidents.md#drafts): nobody
outside sees it and nobody is told until a person publishes it from the
internal board, the admin page, the API or the CLI. There is no publish
tool. Give an agent's token `incident:draft` rather than `incident`, and
it cannot publish at all.

| Tool | Route | Scope | What it answers |
|---|---|---|---|
| `get_status` | `GET /api/v1/status.json` | any token | The page now: overall status, components and their health, open incidents. |
| `list_outages` | `GET /api/v1/outages` | any token; `status:read` adds checks on no component | Outages of checks and heartbeats, newest start first. |
| `get_check_history` | `GET /api/v1/checks/{id}/results` | `status:read` | One check's or heartbeat's raw results, newest first, with errors and assertion detail. |
| `explain_check` | `GET /api/v1/checks/{id}/results?limit=1` | `status:read` | `{check_id, latest}`: the latest result in the past 365 days, or `null`. |
| `get_timeline` | `GET /api/v1/timeline` | any token; `status:read` adds non-public projects and checks on no component | Outages, incidents and deploys, oldest first. |
| `list_incidents` | `GET /api/v1/incidents` | any token; `status:read` adds drafts and internals | Incidents and maintenance, newest first. `external_key` finds every incident with that key, newest first (at most one open), and needs `status:read`. |
| `list_releases` | `GET /api/v1/releases` | any token; `status:read` adds non-public projects | Releases, newest first. |
| `create_incident` | `POST /api/v1/incidents` | `incident` or `incident:draft` | The new incident or maintenance, as a [draft](incidents.md#drafts) unless `draft: false` is passed, which needs `incident`. An [`internal_note` and `external_key`](incidents.md#internal-note-and-external-key) are optional; a key an open incident already has answers that incident and writes nothing; once that incident is resolved, the key opens a new one. The note and key come back only to a token with `status:read`. |
| `update_incident` | `POST /api/v1/incidents/{id}/updates` | `incident`, or `incident:draft` on a draft | The incident with the update appended. `notify: false` (default `true`) posts it without telling subscribers; an update to a draft tells nobody. |
| `discard_incident` | `DELETE /api/v1/incidents/{id}` | `incident` or `incident:draft` | The discarded draft. A published incident is refused. |
| `publish_release` | `POST /api/v1/releases` | `release` | The release, new or updated. |
| `mark_deployed` | `POST /api/v1/releases/{project}/{version}/deployed` | `release` | The release with its deploy time. |
| `validate_config` | none; `tocsin mcp` only | none | `tocsin config check --json` on a folder: `ok` and what it holds, or `ok: false` with every error. |

"Any token" is any known token on `/mcp`, and no token at all through
`tocsin mcp`. Arguments and fields are the route's query parameters and
body, described in each tool's input schema and in [api.md](api.md),
[incidents.md](incidents.md#http-api) and [releases.md](releases.md#http-api).
Timestamps are RFC 3339.

`validate_config` reads a folder on the machine `tocsin mcp` runs on, with
that process's environment, so a `${ENV:...}` secret it cannot see is
reported as an error. A folder that does not validate is a successful call
whose result says `ok: false`; the agent fixes the files and calls again.

### Lists are bounded

Every list tool takes `limit`, with the route's default and largest page.
A limit past the largest is held to it rather than refused, and anything
below 1 is 1.

| Tool | Default | Largest |
|---|---|---|
| `list_outages`, `get_check_history`, `get_timeline` | 100 | 500 |
| `list_incidents` | 20 | 100 |
| `list_releases` | 50 | 200 |

Use the result's cursor (`next_before`, or `next_after` on the timeline) as
`before` or `after` for the next page.

### Errors

A tool that fails returns a tool error, not a protocol error, whose
structured content is the API's answer:

```json
{ "status": 403, "message": "token oncall does not have the `status:read` scope", "fields": [] }
```

`fields` lists each invalid input with its `path`, as a 422 does. Through
`tocsin mcp` the message also names the request that failed, and `status`
is `null` when the server never answered (it could not be reached, or the
tool needs a token and has none).

## Resources

| URI | What |
|---|---|
| `tocsin://schema/config` | The JSON Schema for one configuration file, as `tocsin config schema` prints it. |
| `tocsin://schema/openapi` | The OpenAPI document, as `tocsin openapi` prints it. |

`tocsin mcp` serves both from its own binary, so they describe that
version of tocsin.

## How `/mcp` runs

`/mcp` is stateless: each POST is one JSON-RPC exchange answered with one
JSON response. There are no sessions and no event streams, so `GET /mcp`
and `DELETE /mcp` are 405, server-to-client notifications are not sent, and
an agent's connection is drained on shutdown like any other request,
never holding the 10 second drain. Request bodies are capped at the API's
limit.
