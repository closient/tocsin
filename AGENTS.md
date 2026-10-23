# AGENTS.md

Instructions for coding agents (and humans) working in this repository. [DESIGN.md](DESIGN.md) is the canonical design; read it before adding a feature.

## Build, test, lint

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use.

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check            # licences must stay AGPL-compatible (deny.toml)
```

CI (`.github/workflows/ci.yml`) runs exactly these on linux x86_64 and aarch64, plus a DCO check that every commit in a pull request is signed off. Warnings fail the build. tocsin targets Unix-like systems (Linux, macOS) only; a non-Unix build stops with a `compile_error!`.

## Crate map

| Crate | Holds | Must not |
|---|---|---|
| `crates/tocsin` | the binary: CLI (clap), server, output rendering, exit codes | hold domain logic other crates need |
| `crates/tocsin-core` | domain types, config model, assertions | do I/O |
| `crates/tocsin-store` | SQLite persistence of history | store configuration |
| `crates/tocsin-engine` | the scheduler and state engine: runs checks, confirms state changes, keeps outages, derives component status, emits change events; behind ports (`Runner`, `History`, `Notices`, `Sink`, `Clock`) | hold the rules themselves (pure functions in `tocsin-core::state`) or know about HTTP |
| `crates/tocsin-alert` | operator alerting: routes, the notifiers, and the delivery worker that drains the store's outbox | decide whether a change alerts (the engine does) or consult maintenance windows |

## Conventions

- Every source file starts with `// SPDX-License-Identifier: AGPL-3.0-only`.
- `unsafe` is forbidden workspace-wide (`[workspace.lints.rust]`).
- Files are the only configuration; the database holds history. Never add a code path that writes configuration.
- Secrets come from environment variables (`${ENV:NAME}` in config), never from files in the repo.
- Every email tocsin sends uses a From or Reply-To address a person reads. Never a `noreply` address.
- Every alert has a matching resolve with the same identity.
- Commits are signed off (`git commit -s`); see CONTRIBUTING.md.

## CLI output contract

- With `--json`, exactly one JSON document goes to stdout. Logs always go to stderr (`TOCSIN_LOG` sets the filter, default `info`).
- Errors under `--json` are `{"error": {"kind": "...", "message": "..."}}`; `--json --help` is `{"help": "..."}`.
- A reader that closes the pipe early never changes the exit code: output writes never panic.
- Exit codes are a public contract and never change meaning:

| Code | Kind | Meaning |
|---|---|---|
| 0 | `ok` | the command did what was asked |
| 1 | `check_failed` | the command ran and a check it ran failed |
| 2 | `invalid_input` | bad arguments, or configuration that does not validate |
| 3 | `runtime_error` | anything else: I/O, runtime, database |

## Running things

```sh
tocsin --version --json
tocsin config check [--config <dir>] --json  # validate; every error with file, line, column, path, hint
tocsin config show [--config <dir>] --json   # the merged configuration, secrets redacted
tocsin config schema                         # JSON Schema for one config file
tocsin serve [--config <dir>] [--data <dir>] # until SIGINT/SIGTERM; --data defaults to [server] data_dir
tocsin db info --data <dir> --json           # read-only: size, schema version, row counts, oldest result
tocsin db rollup --data <dir> --since YYYY-MM-DD
tocsin db vacuum --data <dir>                # refused while serve runs on <dir>
tocsin probe run <check> [--config <dir>] --json # one check, once: facts and each assertion's verdict
tocsin openapi                               # the HTTP API's OpenAPI document
tocsin incident new|update|edit|show|list|export ...  # HTTP client: TOCSIN_URL, TOCSIN_TOKEN (or --url, --token)
tocsin release publish|deployed ...                    # HTTP client, as above; docs/releases.md
tocsin maintenance schedule --start <rfc3339> --end <rfc3339> ...
```

`--config` defaults to `/etc/tocsin`; a folder with no `*.toml` file is an error (exit 2).

The configuration format is documented in [`docs/config.md`](docs/config.md); [`examples/config/`](examples/config/) is a working example. After changing a config type, regenerate the committed schema with `cargo run -- config schema > schema/tocsin.schema.json` (a test fails until you do).

The storage schema and its rules are in [`docs/storage.md`](docs/storage.md); `serve` keeps `[server] retention_days` of raw results.

`tocsin probe run` needs no server or database; it exits 0 when the check is up, 1 when it is degraded or down, 2 for an unknown check or invalid configuration, and 3 when the probe cannot run on this machine. Its output, each probe's facts and the ICMP permissions are documented in [`docs/probes.md`](docs/probes.md); the JSON shape is pinned by the snapshots in `crates/tocsin/tests/snapshots/`.

`serve` answers HTTP on `[server] listen`. The incident API, CLI, export format and token setup are in [`docs/incidents.md`](docs/incidents.md). Releases, the timeline API, `/releases` and `/admin/timeline` are in [`docs/releases.md`](docs/releases.md). After changing an API type or route, regenerate the committed document with `cargo run -- openapi > schema/openapi.json` (a test fails until you do). New routes register in `crates/tocsin/src/http/openapi.rs`; writes take the `Authorized<scope::...>` extractor from `http/auth.rs`.

Server-rendered pages (the admin page now) live in `crates/tocsin/src/web/`: askama templates in `crates/tocsin/templates/`, assets compiled in from `crates/tocsin/static/` and served by content hash, and the `Admin` / `AdminForm` extractors for a signed-in person. [`docs/admin.md`](docs/admin.md) has the pattern for a new page and the `[server.auth]` threat model.

Operator alerting, its outbox, each notifier's payload and the webhook signature scheme are in [`docs/alerting.md`](docs/alerting.md). Notifier tests run against loopback mock servers and never touch the network; backoff and repeat tests use paused tokio time.
