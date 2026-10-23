# Quick start

From nothing to a monitor running one check, in five minutes. Install the
binary first ([install.md](install.md)); on Linux x86_64:

```sh
curl -fsSL https://github.com/closient/tocsin/releases/latest/download/tocsin-x86_64-unknown-linux-musl.tar.xz |
  tar -xJ --strip-components=1 tocsin-x86_64-unknown-linux-musl/tocsin
./tocsin --version
```

## 1. Write the configuration

tocsin reads every `*.toml` file in one folder and merges them. One file is
enough to start:

```sh
mkdir -p config
cat > config/tocsin.toml <<'EOF'
#:schema https://github.com/closient/tocsin/releases/latest/download/tocsin.schema.json

[server]
listen = "127.0.0.1:8080"
data_dir = "../data"            # relative to this folder

[page]
title = "Example Status"

[[component]]
id = "website"
name = "Website"

[[check]]
id = "homepage"
component = "website"
type = "http"
target = "https://example.com/"
interval = "1m"

[[check.assert]]
status = [200]

[[check.assert]]
latency_ms = { lt = 2000 }
on_fail = "degraded"
EOF
```

The `#:schema` line gives editors that read it (taplo, Even Better TOML)
completion and validation as you type. Every key is in
[config-reference.md](config-reference.md); [config.md](config.md)
explains them.

## 2. Validate it

```sh
./tocsin config check --config config
```

```text
configuration in config is valid: 1 file; 1 check, 1 component, 0 groups, 0 heartbeats, 0 notifiers, 0 projects, 0 routes, 0 tokens
```

An invalid file exits 2 and names the file, line, column and key, with a
hint. Add `--json` for the same as one JSON document.

## 3. Run the check once

```sh
./tocsin probe run homepage --config config
```

```text
check `homepage` (http https://example.com/) is up after 59 ms
facts:
  ...
  latency_ms: 56
  status: 200
assertions:
  pass status: expected [200], actual 200
  pass latency_ms: expected {"lt":2000}, actual 56
```

It needs no server or database. Exit 0 means up, 1 degraded or down, 3 the
probe could not run here. [probes.md](probes.md) describes every fact and
assertion.

## 4. Serve

```sh
./tocsin serve --config config
```

The server runs every check on its interval and records the results in
`data/tocsin.db`. Stop it with Ctrl-C. While it runs:

```sh
curl -s localhost:8080/api/v1/timeline            # outages, incidents and releases
curl -s localhost:8080/api/v1/openapi.json        # the whole HTTP API
./tocsin db info --data data --json               # what the database holds
```

## Next

- Alert someone: `[[notifier]]` and `[[route]]` in [alerting.md](alerting.md).
- Watch cron jobs: [heartbeats.md](heartbeats.md).
- Record releases from CI: [release-ci.md](release-ci.md).
- Run it for real: the container image or the systemd unit in
  [install.md](install.md), behind Cloudflare in
  [cloudflare-access.md](cloudflare-access.md).
- [examples/config/](../examples/config/) is a fuller configuration, split
  one file per concern.
