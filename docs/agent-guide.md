# Operating tocsin with a coding agent

tocsin is built to be run by an agent as much as by a person: the whole
setup is a folder of TOML files, every command can answer in JSON, and exit
codes never change meaning. This page is the contract an agent can rely on.
(Changing tocsin's own code is [AGENTS.md](../AGENTS.md).)

## The loop

1. **Edit TOML.** Files are the only configuration: there is no settings UI
   and no config API. Put the files under version control and change them
   there.
2. **Validate** with `tocsin config check --config <dir> --json`.
3. **Try new checks** with `tocsin probe run <check-id> --config <dir> --json`
   before shipping them.
4. **Ship** the files. tocsin reloads the folder by itself, or at once on
   SIGHUP (`systemctl reload tocsin`). A file that does not validate
   changes nothing: confirm with `tocsin config status --json`, which
   exits 1 with the errors when the last reload failed. A change to
   `[server]` or to the environment still needs a restart.

## What to read

| Need | Source |
|------|--------|
| Every key, type, bound and default | `tocsin config schema` (JSON Schema, draft 7), or [config-reference.md](config-reference.md), generated from it |
| What the keys mean, with examples | [config.md](config.md) |
| A working configuration to copy | [examples/config/](../examples/config/) |
| The HTTP API | `tocsin openapi`, or `GET /api/v1/openapi.json` on a running server; [api.md](api.md) |
| Tools instead of commands, over MCP | `/mcp` on a running server, or `tocsin mcp`; [mcp.md](mcp.md) |
| What a probe observes and each assertion reads | [probes.md](probes.md) |

Point editors and language servers at the schema with a first line of
`#:schema ./tocsin.schema.json` in each file, after
`tocsin config schema > tocsin.schema.json`.

## JSON output

With `--json`, stdout is exactly one JSON document and logs go to stderr
(`TOCSIN_LOG` sets the filter). Parse stdout; never scrape the text form.
A failure is:

```json
{"error": {"kind": "invalid_input", "message": "no check has the id `homepag`; did you mean `homepage`?"}}
```

`config check` adds every error with its location and a hint, so one run
finds them all:

```json
{"error": {"kind": "invalid_input", "message": "configuration in config has 1 error", "errors": [
  {"file": "config/checks.toml", "line": 9, "column": 1, "path": "check[0].intervall",
   "message": "unknown field `intervall`, expected one of `id`, `name`, ...", "hint": "did you mean `interval`?"}
]}}
```

## Exit codes

| Code | `kind` | Meaning | What to do |
|------|--------|---------|------------|
| 0 | `ok` | Done as asked. | |
| 1 | `check_failed` | It ran, and a check it ran failed. | Read `assertions` in the output: the target is unhealthy, or the assertion is wrong. For `config status`, the last reload failed: read `errors`, fix the files, and the next reload recovers. |
| 2 | `invalid_input` | Bad arguments or configuration; for client commands, any 4xx from the server. | Fix the input; retrying will not help. |
| 3 | `runtime_error` | Anything else: I/O, the database, a probe that cannot run on this machine, a server that is down or answers 5xx. | The environment, not the input. For `probe run`, exit 3 says nothing about the target. |

## Rules that save a round trip

- **Secrets never go in the files.** Write `"${ENV:NAME}"`; tocsin reads the
  variable when it loads. `config check` needs every named variable set:
  for a dry run, set placeholders.
- **Ids are unique per kind across every file**, and are how everything
  refers to everything: a check's `component`, a route's `notifiers`, a
  token's id in the audit trail.
- **Every `*.toml` under the folder is read**, subfolders included, in
  sorted order. Split by concern; a single table such as `[server]` defined
  in two files is an error naming both places.
- **`timeout` must be shorter than `interval`.**
- **Every email can be answered.** A `noreply`-style From or Reply-To is
  rejected.
- **Client commands are idempotent where it matters.** `release publish`,
  `release deployed` and `heartbeat ping` can be retried after a failure.
  `incident new` is, given `--external-key`: a second create with a key
  some incident has answers that incident and writes nothing. Without a
  key, check `incident list --json` first.
- **What an agent writes about an incident is a draft.** Pass `--draft` to
  `incident new` (MCP's `create_incident` drafts by default) and leave
  publishing to a person; a token with only `incident:draft` cannot
  publish. A draft is invisible to the public and tells nobody, and
  `incident discard <id>` removes one written by mistake. Evidence, host
  names and ticket links go in `--internal-note`, which no public surface
  ever shows, never in the title or an update. See
  [incidents.md](incidents.md#drafts) and
  [its internal note](incidents.md#internal-note-and-external-key).
- **`config show --json`** prints the merged configuration with secrets
  redacted, which is the safe way to show a person what tocsin sees.

## Reading state

| Question | Command |
|----------|---------|
| Is this check passing right now, from here? | `tocsin probe run <id> --json` |
| What happened recently? | `GET /api/v1/timeline` (outages, incidents, releases) |
| What incidents are open? | `tocsin incident list --json` |
| Did my configuration change go in? | `tocsin config status --json` (needs a `status:read` token) |
| How big is the database, and how old? | `tocsin db info --data <dir> --json` |

Client commands (`incident`, `maintenance`, `release`, `heartbeat`) take the
server from `TOCSIN_URL` and a token from `TOCSIN_TOKEN`. Prefer the
environment variables to `--token`: arguments show in `ps`.
