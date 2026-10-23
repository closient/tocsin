# Prometheus metrics

`GET /metrics` serves what tocsin knows in the Prometheus text format
(version 0.0.4): each check's and heartbeat's confirmed health, how long
probes take, each component's status, notifier failures, how the
configuration last reloaded, and the database's size. Point Prometheus at
it to graph tocsin and to alert through Alertmanager on what tocsin sees,
or on tocsin itself.

## Scraping

The endpoint needs a token with the `status:read` scope. With no token, or
one that names no `[[token]]`, it answers 401; with a token that lacks the
scope, 403. The series name every check, component and notifier, and a
check's id often names an internal host, which is more than the public
status page shows, so the endpoint is not public.

```toml
[[token]]
id = "prometheus"
secret = "${ENV:TOCSIN_PROMETHEUS_TOKEN}"
scopes = ["status:read"]
description = "Prometheus scraping /metrics"
```

```yaml
scrape_configs:
  - job_name: tocsin
    scheme: https
    metrics_path: /metrics
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/tocsin-token
    static_configs:
      - targets: ["status.example.com"]
```

Each scrape reads the state at that moment. Nothing is computed per
scrape beyond that, so the usual 15 to 60 second interval is fine.

## Labels

Every label value comes from the configuration, never from a result, so
the number of series is fixed by what is configured:

- `check`: a `[[check]]` or `[[heartbeat]]` id.
- `component`: the check's `component`; empty, and so absent in
  Prometheus, for a check without one.
- `node`: this server's `node_id`, the hostname unless `[server]` sets
  it. It tells nodes apart when several tocsins feed one Prometheus.
- `kind`: `check` or `heartbeat`.
- `status`: one of the five component statuses.
- `notifier`: a `[[notifier]]` id.

When a reload removes a check, heartbeat, component or notifier, its
series go from the next scrape. Prometheus marks them stale, so an alert
on a removed check resolves rather than firing on forever.

## Families

| Family | Type | Labels | Meaning |
|---|---|---|---|
| `tocsin_check_up` | gauge | `check`, `component`, `node`, `kind` | Confirmed health: 1 up, 0.5 degraded, 0 down. |
| `tocsin_check_duration_seconds` | histogram | `check`, `component`, `node` | How long each check's probes take. |
| `tocsin_check_last_run_timestamp` | gauge | `check`, `component`, `node` | When a check's latest probe started, unix seconds. |
| `tocsin_heartbeat_last_ping_timestamp` | gauge | `check`, `component`, `node` | When a heartbeat was last pinged, unix seconds. |
| `tocsin_component_status` | gauge | `component`, `node`, `status` | 1 for the status each component shows, 0 for the other four. |
| `tocsin_notification_failures_total` | counter | `notifier` | Failed attempts to deliver an alert message, retried or not. |
| `tocsin_notifications_abandoned_total` | counter | `notifier` | Alert messages given up on after their last attempt failed. |
| `tocsin_config_reload_success` | gauge | | 1 if the last reload succeeded, 0 if it failed and the last good configuration is still running. |
| `tocsin_config_last_error_timestamp` | gauge | | When a reload last failed, unix seconds; 0 if none has. |
| `tocsin_db_size_bytes` | gauge | | The SQLite database's size on disk, its write-ahead log included. |
| `tocsin_build_info` | gauge | `version` | Always 1; `version` is the running tocsin's. |

Some series are absent until there is something to say:

- `tocsin_check_up` appears once a check's first state is confirmed,
  which takes `confirm` agreeing results (see
  [the engine](engine.md#results-and-confirmation)). It is the same state
  that opens outages and fires alerts, not the latest raw result.
- `tocsin_check_duration_seconds` appears after a check's first probe
  since tocsin started, and `tocsin_check_last_run_timestamp` once the
  check has a result. Heartbeats have neither; they do not probe.
- `tocsin_heartbeat_last_ping_timestamp` appears after the first ping
  since tocsin started. Until then use `tocsin_check_up{kind="heartbeat"}`,
  which goes to 0 when a deadline passes.
- The notifier counters appear after a notifier's first failure. Wrap
  them in `increase()` or `rate()`, which treat a series that appears as
  starting from zero.

The duration buckets run from 5 ms to 60 s: `0.005, 0.01, 0.025, 0.05,
0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60`.

The counters and the histogram count from zero when tocsin starts, as
Prometheus expects.

## Alert rules

[`examples/prometheus/tocsin-rules.yml`](../examples/prometheus/tocsin-rules.yml)
holds rules to start from, with unit tests in
[`tocsin-rules.test.yml`](../examples/prometheus/tocsin-rules.test.yml)
that CI runs through `promtool test rules`:

```yaml
groups:
  - name: tocsin
    rules:
      - alert: TocsinCheckDown
        expr: tocsin_check_up{kind="check"} == 0
        labels:
          severity: page
        annotations:
          summary: "{{ $labels.check }} is down on {{ $labels.node }}"

      - alert: TocsinCheckDegraded
        expr: tocsin_check_up{kind="check"} == 0.5
        for: 15m
        labels:
          severity: ticket

      - alert: TocsinHeartbeatMissed
        expr: tocsin_check_up{kind="heartbeat"} == 0
        labels:
          severity: page

      - alert: TocsinNotificationsAbandoned
        expr: increase(tocsin_notifications_abandoned_total[15m]) > 0
        labels:
          severity: page

      - alert: TocsinConfigReloadFailed
        expr: tocsin_config_reload_success == 0
        for: 5m
        labels:
          severity: ticket

      - alert: TocsinUnreachable
        expr: up{job="tocsin"} == 0
        for: 5m
        labels:
          severity: page
```

`tocsin_check_up` is already confirmed, so `TocsinCheckDown` needs no
`for:`; adding one delays the page by that much on top of `confirm`.
Keep `TocsinUnreachable` whatever else you use: none of the other rules
can fire while Prometheus cannot reach tocsin.

Other queries worth having:

- `histogram_quantile(0.95, sum by (check, le) (rate(tocsin_check_duration_seconds_bucket[5m])))`:
  each check's 95th percentile probe time.
- `time() - tocsin_check_last_run_timestamp > 600`: checks that have not
  run in ten minutes, longer than any sensible interval.
- `rate(tocsin_notification_failures_total[15m]) > 0`: a notifier failing
  before any message is abandoned.
- `predict_linear(tocsin_db_size_bytes[1d], 30 * 86400) > 10e9`: a
  database on course for 10 GB within a month.
