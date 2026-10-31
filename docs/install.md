# Installing tocsin

tocsin is one static binary. Each `v*` tag publishes it three ways: release
archives, a container image, and a systemd unit to run the binary with.

## Release archives

Every [GitHub Release](https://github.com/closient/tocsin/releases) carries:

| File | |
|------|-|
| `tocsin-x86_64-unknown-linux-musl.tar.xz` | Linux x86_64, statically linked: runs on any distribution. |
| `tocsin-aarch64-unknown-linux-musl.tar.xz` | Linux arm64, statically linked. |
| `tocsin-aarch64-apple-darwin.tar.xz` | macOS on Apple silicon. |
| `<archive>.sha256` | The SHA-256 of one archive. |
| `sha256.sum` | Every published file's SHA-256, in `sha256sum` format. |
| `tocsin.schema.json` | The JSON Schema for one config file, as `tocsin config schema` prints it. |
| `openapi.json` | The HTTP API's OpenAPI document, as `tocsin openapi` prints it. |
| `documents.sha256` | The SHA-256 of the two documents above. |
| `source.tar.gz` | The source the release was built from. |

Each archive holds one folder, `tocsin-<target>/`, with the binary, the
README and the licence. Download, verify and install:

```sh
version=v0.9.0                       # the release you want
name=tocsin-x86_64-unknown-linux-musl
base=https://github.com/closient/tocsin/releases/download/$version
curl -fsSLO "$base/$name.tar.xz" -fsSLO "$base/$name.tar.xz.sha256"
sha256sum --check --strict "$name.tar.xz.sha256"
tar -xJf "$name.tar.xz"
sudo install -m 0755 "$name/tocsin" /usr/local/bin/tocsin
tocsin --version
```

On macOS use `shasum -a 256 -c` in place of `sha256sum --check`. To check
the schema and the OpenAPI document, download them with
`documents.sha256` and run `sha256sum --check documents.sha256` in that
folder.

From source, with the Rust toolchain pinned in `rust-toolchain.toml`:
`cargo install --locked --git https://github.com/closient/tocsin tocsin`.

## Container image

```sh
docker pull ghcr.io/closient/tocsin:latest
```

Tags are the version without its `v` (`0.9.0`), its minor series (`0.9`)
and `latest`, for `linux/amd64` and `linux/arm64`. Prereleases are not
published as images. The image is built from the release's own musl
binaries on `gcr.io/distroless/static-debian13:nonroot`: no shell, no
package manager, and the process runs as uid and gid 65532.

| | |
|-|-|
| Entrypoint | `/usr/local/bin/tocsin` |
| Default command | `serve --config /config --data /data` |
| `/config` | The config folder; mount yours read-only. |
| `/data` | The SQLite database. Owned by 65532 in the image, so a named volume works as is. |
| Port | 8080; set `[server] listen = "0.0.0.0:8080"`, since the default listens on loopback only. |

```sh
docker volume create tocsin-data
docker run -d --name tocsin -p 8080:8080 \
  -v "$PWD/config:/config:ro" -v tocsin-data:/data \
  --env-file tocsin.env \
  ghcr.io/closient/tocsin:latest

docker run --rm -v "$PWD/config:/config:ro" --env-file tocsin.env \
  ghcr.io/closient/tocsin:latest config check --config /config
```

`tocsin.env` holds the secrets your config names with `${ENV:...}`. A bind
mount for `/data` must be writable by uid 65532
(`sudo chown 65532:65532 data`); otherwise `serve` exits 3 with
`cannot open lock file /data/tocsin.lock: Permission denied`.

### ICMP checks in a container

The process is not root and has no capabilities, so an `icmp` check uses an
unprivileged ICMP socket. That needs the container's
`net.ipv4.ping_group_range` to include gid 65532:

- **Docker** sets it to `0 2147483647` in every container's network
  namespace, so ICMP works with no flags. Another runtime may not: check
  with `cat /proc/sys/net/ipv4/ping_group_range` inside the container, and
  pass `--sysctl net.ipv4.ping_group_range="0 2147483647"` if needed.
- **Kubernetes** does not; set it in the pod's security context. It is a
  safe sysctl, allowed by default:

  ```yaml
  securityContext:
    sysctls:
      - name: net.ipv4.ping_group_range
        value: "0 2147483647"
  ```

The other method is a raw socket, which needs `CAP_NET_RAW`. Granting it
to a non-root process in a container does not work on its own: Docker's
`--cap-add` sets the bounding set, not the ambient capabilities a non-root
process starts with. It works only for root: `--user 0:0` keeps Docker's
default `NET_RAW`, but gives up the non-root user. Prefer the sysctl.

With neither available, an `icmp` check fails with an error naming both,
and `tocsin probe run` exits 3. [probes.md](probes.md#icmp) has the
details outside containers.

## systemd

[`deploy/tocsin.service`](../deploy/tocsin.service) runs the binary at
`/usr/local/bin/tocsin` with config in `/etc/tocsin` and the database in
`/var/lib/tocsin`.

```sh
sudo install -m 0755 tocsin /usr/local/bin/tocsin
sudo install -d -m 0755 /etc/tocsin
sudo cp config/*.toml /etc/tocsin/
sudo install -m 0600 /dev/null /etc/tocsin/tocsin.env   # then add TOCSIN_SMTP_URL=... and the rest
sudo sh -c 'set -a; . /etc/tocsin/tocsin.env; tocsin config check --config /etc/tocsin'
sudo install -m 0644 tocsin.service /etc/systemd/system/tocsin.service
sudo systemctl daemon-reload
sudo systemctl enable --now tocsin
journalctl -u tocsin -f
```

- **Secrets** go in `/etc/tocsin/tocsin.env`, one `NAME=value` per line,
  read by systemd as root before the service starts; keep it mode 0600.
  tocsin itself never reads the file, and `.env` is not `*.toml`, so it is
  not mistaken for configuration.
- **The user** is a `DynamicUser`: systemd allocates an unprivileged uid
  for each start. The database is in `/var/lib/private/tocsin`, reached
  through the `/var/lib/tocsin` link, and survives restarts and upgrades.
- **ICMP** works through `AmbientCapabilities=CAP_NET_RAW`, the only
  capability the unit grants. To run without it, remove both
  `AmbientCapabilities=` and `CapabilityBoundingSet=` and set
  `net.ipv4.ping_group_range = 0 2147483647` instead: the full range,
  because a dynamic user's gid is not known in advance.
- **Configuration changes** apply by themselves: tocsin reloads
  `/etc/tocsin` half a second after the last file changes, and
  `systemctl reload tocsin` (a SIGHUP) reloads it at once. A change to
  `tocsin.env` or to `[server]` needs `systemctl restart tocsin`, since
  the environment is read once at start. See
  [config.md](config.md#reloading).

The unit is hardened: a read-only file system except its state directory,
no home directories, private `/tmp` and devices, no kernel tunables,
modules or logs, only `AF_INET`, `AF_INET6` and `AF_UNIX` sockets, and a
system-call filter of `@system-service` minus `@privileged` and
`@resources`. `systemd-analyze security tocsin.service` rates it **1.4 OK**.
What remains is what the job needs: the network and `CAP_NET_RAW`. CI
installs the unit on every pull request, runs `systemd-analyze verify` and
`systemd-analyze security --threshold=20` (fails above 2.0), starts it, and runs an ICMP
check under it with and without the capability.
