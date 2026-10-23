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

CI (`.github/workflows/ci.yml`) runs exactly these on linux x86_64 and aarch64. Warnings fail the build.

## Crate map

| Crate | Holds | Must not |
|---|---|---|
| `crates/tocsin` | the binary: CLI (clap), server, output rendering, exit codes | hold domain logic other crates need |
| `crates/tocsin-core` | domain types, config model, assertions | do I/O |
| `crates/tocsin-store` | SQLite persistence of history | store configuration |

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
- Errors under `--json` are `{"error": {"kind": "...", "message": "..."}}`.
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
tocsin serve --config <dir> --data <dir>    # until SIGINT/SIGTERM
```

One-shot probes (`tocsin probe run <check> --json`, which runs a single check and explains its verdict) are not implemented yet; they arrive with the probe engine.
