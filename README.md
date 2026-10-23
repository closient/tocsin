# tocsin

A single-binary uptime monitor, public status page and release log, written in Rust.

- **Files are the only configuration.** A folder of TOML files says what is watched, who is alerted and how the page looks. There is no settings UI and no config API, so git, Ansible or a coding agent is the whole management story.
- **SQLite holds history.** Check results, outages, incidents and releases live in one database file in a data folder.
- **Releases on the same timeline as outages.** CI pushes each release and deploy, so "what changed right before this broke" is one glance.
- **Built for coding agents.** Schema-validated config, `--json` on every command, stable exit codes, and one-shot probe runs that explain their verdict.

tocsin is early: the workspace and CLI skeleton exist, and the monitor is being built now. [DESIGN.md](DESIGN.md) is the plan.

## Quick start

```sh
cargo build --release
mkdir -p config
./target/release/tocsin serve --config config --data data
```

`tocsin --version --json` prints `{"version": "..."}`. Stop the server with Ctrl-C or SIGTERM.

## License

[AGPL-3.0-only](LICENSE). If you run a modified tocsin as a network service, you must offer its source to the people using it. Contributions are accepted under the same license with a DCO sign-off; see [CONTRIBUTING.md](CONTRIBUTING.md).
