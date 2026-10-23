# tocsin documentation

## Start

- [Quick start](quickstart.md): one check running in five minutes.
- [Installing](install.md): release archives and their checksums, the
  container image, and the hardened systemd unit.
- [Running behind Cloudflare](cloudflare-access.md): a Tunnel, a public
  page, and the admin page behind Access.

## Configure

- [Configuration](config.md): every table, with examples.
- [Configuration reference](config-reference.md): every key, type, bound
  and default, generated from the JSON Schema.
- [Probes and assertions](probes.md): what each check type observes,
  which assertions apply, and `tocsin probe run`.
- [Heartbeats](heartbeats.md): watching jobs that report in.
- [Alerting](alerting.md): notifiers, routes and their payloads.

## Use

- [The status page](status-page.md): what `/` shows, live updates, the
  incident archive, Atom feeds and caching behind a CDN.
- [Incidents and maintenance](incidents.md).
- [Releases and the timeline](releases.md), and
  [recording them from CI](release-ci.md).
- [Email subscribers](subscribers.md): sign-up with double opt-in, what
  is sent, one-click unsubscribe, and managing subscribers.
- [The admin page](admin.md): signing in through a proxy, and its threat
  model.
- [HTTP API](api.md), described by the OpenAPI document
  [`schema/openapi.json`](../schema/openapi.json); a running server serves
  it at `/api/v1/openapi.json`, and `tocsin openapi` prints it.
- [Prometheus metrics](metrics.md): `/metrics`, its labels, and alert
  rules to start from.
- [Operating tocsin with a coding agent](agent-guide.md): the JSON and
  exit-code contract, and the edit, check, probe loop.
- [MCP server](mcp.md): `/mcp` and `tocsin mcp` for Claude Code and other
  MCP clients, every tool and the scope each needs.

## Internals

- [The engine](engine.md): scheduling, confirmation and component state.
- [Storage](storage.md): the SQLite schema and retention.
