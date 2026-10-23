# Probes

A check's probe gathers facts about its target; the check's [assertions](config.md#assertions-checkassert) then decide whether it is `up`, `degraded` or `down`. `tocsin probe run` runs one check once, without the server or the database, and shows both:

```sh
tocsin probe run <check-id> [--config /etc/tocsin] [--json]
```

`--config` defaults to `/etc/tocsin`. The check runs with the timeout and settings it has in the configuration, and trusts the operating system's root certificates.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | The check is `up`. |
| 1 | The check is `degraded` or `down`. The output says why. |
| 2 | Invalid input: no check has that id (the error suggests the nearest one), or the configuration does not validate. |
| 3 | The probe could not run on this machine, such as an ICMP socket the process may not open, or no root certificates for an `http` or `tls` check. This says nothing about the target. |

## Text output

```text
check `statuspage-cdn` (http http://127.0.0.1:8080/api/v2/components.json) is degraded after 3 ms
facts:
  body: {\n  "page": {"id": "y2j98763l1zq", "name": "Example", "url": "https://status.example.com"},\n  "components": [\n    {"id": "b13yz5g2cw10", "name": "API", "status": "operational", "group": false},\n  ... (291 bytes in all)
  body_bytes: 291
  body_truncated: false
  certificate: null
  headers.content-length: 291
  headers.content-type: application/json
  headers.date: Thu, 23 Oct 2026 12:00:00 GMT
  latency_ms: 2
  redirect_location: null
  status: 200
  url: http://127.0.0.1:8080/api/v2/components.json
assertions:
  FAIL json: expected {"eq":"operational","path":"$.components[?@.name=='CDN'].status"}, actual "degraded_performance" -> degraded
  pass body: expected {"contains":"\"page\""}, actual "{\n  \"page\": {\"id\": \"y2j98763l1zq\", \"name\": \"Example\", \"url\": \"https://status.example.com\"},\n  \"components\": [\n    {\"id\": \"b13yz5g2cw10\", \"name\": \"API\", \"status\": \"operat...
```

Long values are cut at 200 characters in text output. The JSON output keeps facts whole, up to the probe's own limits.

## JSON output

With `--json`, stdout is exactly one JSON document and logs go to stderr. When the probe ran (exit 0 or 1), the document is the outcome:

```json
{
  "check": "statuspage-cdn",
  "type": "http",
  "target": "http://127.0.0.1:8080/api/v2/components.json",
  "started_at_ms": 1792756800000,
  "duration_ms": 3,
  "health": "degraded",
  "facts": {
    "type": "http",
    "latency_ms": 2,
    "url": "http://127.0.0.1:8080/api/v2/components.json",
    "status": 200,
    "redirect_location": null,
    "headers": {"content-type": ["application/json"], "content-length": ["291"]},
    "body": "{\"page\": ...}",
    "body_bytes": 291,
    "body_truncated": false,
    "certificate": null
  },
  "assertions": [
    {
      "condition": "json",
      "expected": {"path": "$.components[?@.name=='CDN'].status", "eq": "operational"},
      "actual": "degraded_performance",
      "passed": false,
      "on_fail": "degraded",
      "implicit": false
    }
  ],
  "error": null
}
```

| Key | |
|-----|-|
| `check`, `type`, `target` | The check, as configured. |
| `started_at_ms` | When the probe started, in milliseconds since the Unix epoch. |
| `duration_ms` | How long the probe took, in milliseconds. |
| `health` | `up`, `degraded` or `down`. |
| `facts` | What the probe observed, tagged by `type`; `null` when the probe could not complete. |
| `assertions` | Each assertion's verdict, in configuration order; empty when the probe could not complete. |
| `error` | `null`, or `{"kind", "message"}` when the probe could not complete; the check is then `down`. |

Each verdict has `condition` (the assertion's key, such as `status` or `json`), `expected` (the assertion as configured), `actual` (what the probe found; text longer than 256 characters is cut and ends in `…`), `passed`, `on_fail` (`down` or `degraded`), and `implicit` (true for the [default conditions](config.md#assertions-checkassert) a check with no assertions gets). A verdict that could not be evaluated, such as a `json` assertion on a body that is not JSON, has `passed: false` and a `message` saying why.

An error's `kind` is one of:

| `kind` | |
|--------|-|
| `timeout` | The check's `timeout` passed. |
| `connect` | The connection could not be made: refused, unreachable, or the host did not resolve. |
| `tls` | The TLS handshake failed, including a certificate that does not verify when `verify_certificate` is on. |
| `dns` | The lookup failed without an answer from the resolver. |
| `protocol` | The server answered with something that is not the protocol. |

Failures before the probe runs (exit 2 and 3) are the usual `{"error": {"kind", "message"}}` document, with `kind` `invalid_input` or `runtime_error`.

### Facts by type

| `type` | Keys |
|--------|------|
| `http` | `latency_ms` (sending the request to reading the body), `url` (where redirects led, if followed), `status`, `redirect_location` (the `Location` of a 3xx that was not followed), `headers` (lower-case names, each with every value sent), `body`, `body_bytes`, `body_truncated`, `certificate` (https only) |
| `tls` | `latency_ms` (connecting and the handshake), `certificate` |
| `tcp` | `latency_ms` (connecting), `address`, `banner` (only when read; see below) |
| `dns` | `latency_ms`, `resolver` (`ip:port`, or `system`), `record_type`, `rcode` (such as `NOERROR` or `NXDOMAIN`), `answers` (the records of the type asked for, as text) |
| `icmp` | `latency_ms` (the echo's round trip), `address` |

`certificate` has `server_name` (sent as SNI and checked against), `chain` (each certificate's `subject`, `issuer`, `not_before`, `not_after` and `subject_alternative_names`, the server's own first), `expires_in_seconds`, `days_remaining` (whole days, rounded down, negative once expired; both `null` when the server's certificate cannot be parsed, which fails a `certificate_days_remaining` assertion as unavailable), `chain_valid`, `chain_error` (why the chain does not verify, or `null`) and `hostname_valid`.

## How each probe works

### http

One request, with the method, headers, body and credentials from `[check.http]`. Redirects are not followed unless `follow_redirects = true`; when they are, up to 10 are followed, and the certificate reported is the last server's. At most 64 KiB of the body is read; `body_truncated` says when there was more, and assertions see only what was read. Proxy environment variables are ignored.

With `verify_certificate = true` (the default), a certificate that does not verify fails the probe with a `tls` error. With `false`, the request goes ahead and `certificate` still reports the chain, so `chain_valid` and `hostname_valid` assertions can decide how much that matters.

### tls

A handshake only: no request is sent. The certificate is reported whether or not it verifies, and the assertions judge it; with none, the implicit ones require a valid chain and a matching host name.

### tcp

A connection, timed. With `[check.tcp] send`, that text is written once connected. <a id="tcp-banners"></a>The reply, or the server's greeting when nothing is sent, is read only when a `banner` assertion asks for it, so a check without one never waits on a server that says nothing, even after `send`. Reading stops at the first newline (the newline, and a carriage return before it, are dropped), at 4 KiB, when the server closes the connection, or when nothing more arrives for 250 ms after the first bytes. Waiting for the first bytes counts against the check's `timeout`.

### dns

One query to the configured resolver, or the system's, with no cache and without the hosts file. The target is a complete name: no search domains are tried. Any response code is a completed probe; a check with no assertions requires `NOERROR`. A resolver that does not answer is a `timeout` error.

### icmp

One echo request. tocsin needs permission to send it, in one of two ways:

- **Unprivileged ICMP sockets** (Linux). The kernel allows them to processes whose group is within `net.ipv4.ping_group_range`. Many distributions already allow every group (`0 2147483647`); check, and widen it if needed:

  ```sh
  sysctl net.ipv4.ping_group_range
  echo 'net.ipv4.ping_group_range = 0 2147483647' | sudo tee /etc/sysctl.d/60-tocsin-ping.conf
  sudo sysctl --system
  ```

  In a container, set the sysctl on the container: `docker run --sysctl net.ipv4.ping_group_range="0 2147483647" ...`.

- **`CAP_NET_RAW`**, which allows a raw socket:

  ```sh
  sudo setcap cap_net_raw+ep /usr/local/bin/tocsin
  ```

  Under systemd, `AmbientCapabilities=CAP_NET_RAW` in the unit does the same without changing the binary.

tocsin tries an unprivileged socket first and a raw socket second. With neither allowed, the check fails with an error naming both options, and `tocsin probe run` exits 3.
