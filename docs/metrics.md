# Prometheus metrics

`GET /metrics` serves what tocsin knows in the Prometheus text format
(version 0.0.4): each check's and heartbeat's confirmed health, how long
probes take, each component's status, notifier failures, how the
configuration last reloaded, and the database's size. Point Prometheus at
it to graph tocsin and to alert through Alertmanager on what tocsin sees,
or on tocsin itself. The same metrics can also be pushed to an
OpenTelemetry collector over OTLP; see [OpenTelemetry](#opentelemetry).

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
| `tocsin_alerts_abandoned_total` | counter | | Alerts that ended without their resolve being delivered: the resolve failed for good, most often because its notifier was removed while tocsin was stopped. Not labelled by notifier, since a removed notifier's series are no longer exported; tocsin logs each one at `error` with the alert and notifier. |
| `tocsin_config_reload_success` | gauge | | 1 if the last reload succeeded, 0 if it failed and the last good configuration is still running. |
| `tocsin_config_last_error_timestamp` | gauge | | When a reload last failed, unix seconds; 0 if none has. |
| `tocsin_db_size_bytes` | gauge | | The SQLite database's size on disk, its write-ahead log included. |
| `tocsin_slack_connections` | gauge | `purpose`, `state` | Slack channels connected by signing in: `purpose` is `operator` (alert channels on `/admin/slack`) or `subscriber` (added from the subscribe page), `state` is `live` or `dead` (Slack revoked it). |
| `tocsin_slack_connection_up` | gauge | `notifier`, `connection` | 1 when the Slack `connection` a notifier names is connected and live; 0 when it is missing or dead, so its alerts fail. |
| `tocsin_build_info` | gauge | `version` | Always 1; `version` is the running tocsin's. |
| `tocsin_otlp_export_failures_total` | counter | | Attempts to send an OTLP metric export that failed, retried or not: refused, partly rejected, unanswered within the timeout, or never sent. |

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
- The two Slack families appear only with `[slack]` or a notifier naming
  a Slack `connection`. All four
  `tocsin_slack_connections` series are always there then, at 0 when
  empty; `tocsin_slack_connection_up` has one series per notifier naming a
  `connection`. `TocsinSlackConnectionDown`, below, pages on it at 0:
  every alert sent to a channel nobody connected, or one Slack revoked,
  fails until someone connects it (see
  [alerting.md](alerting.md#when-a-channel-stops-working)).
- The notifier counters appear after a notifier's first failure. Wrap
  them in `increase()` or `rate()`, which treat a series that appears as
  starting from zero.
- `tocsin_otlp_export_failures_total` appears only when
  [`[telemetry.otlp]`](#opentelemetry) is configured, from 0 when the
  export starts. Without the table the page is exactly what it was
  before tocsin could export.

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

      - alert: TocsinAlertsAbandoned
        expr: increase(tocsin_alerts_abandoned_total[15m]) > 0
        labels:
          severity: ticket

      - alert: TocsinSlackConnectionDown
        expr: tocsin_slack_connection_up == 0
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
- `increase(tocsin_otlp_export_failures_total[15m]) > 0`: the OTLP export
  failing, so the collector's copy of these metrics has gaps.

## OpenTelemetry

tocsin can push the same metrics to an OpenTelemetry collector over OTLP,
for a setup that collects through the OpenTelemetry Collector rather than
by scraping. It is off unless `tocsin.toml` has a `[telemetry.otlp]`
table; setting only the `OTEL_*` variables turns nothing on, and with no
table tocsin opens no connection to any collector.

```toml
[telemetry.otlp]
endpoint = "http://otel-collector:4318"
protocol = "http/protobuf"            # or "grpc"
headers = { authorization = "Bearer ${ENV:OTLP_TOKEN}" }
interval = "60s"
resource = { "deployment.environment.name" = "production" }
```

Every key is optional; an empty `[telemetry.otlp]` exports to
`http://localhost:4318` every minute, or wherever the standard variables
say. The table is read at startup, like `[server]`: a reload that changes
it logs a warning and keeps the running exporter until tocsin restarts. `tocsin config check` validates
it, the variables included, and names a bad variable without printing its
value. The keys are described in the
[configuration reference](config-reference.md).

### Which source wins

For each setting tocsin takes the first of these that is set, so the file
always wins over the environment and a metrics-only variable over a
general one:

| Setting | 1. `[telemetry.otlp]` | 2. Metrics variable | 3. General variable | 4. Default |
|---|---|---|---|---|
| Endpoint | `endpoint` + `/v1/metrics` | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, used as is | `OTEL_EXPORTER_OTLP_ENDPOINT` + `/v1/metrics` | `http://localhost:4318/v1/metrics` |
| Protocol | `protocol` | `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` |
| Interval | `interval` | | `OTEL_METRIC_EXPORT_INTERVAL` (ms, at least 1000) | 60 s |
| Timeout | `timeout` | `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` (ms) | `OTEL_EXPORTER_OTLP_TIMEOUT` (ms) | 10 s, or the interval if shorter |

Over `grpc` the endpoint is not given a path, and the default port is
4317. The timeout may not be longer than the interval, so exports never
queue up behind each other.

Headers are merged, not chosen: those in `OTEL_EXPORTER_OTLP_HEADERS`,
then `OTEL_EXPORTER_OTLP_METRICS_HEADERS`, then `headers`, a later source
replacing a header an earlier one named. The resource is merged the same
way: `OTEL_RESOURCE_ATTRIBUTES`, then `OTEL_SERVICE_NAME` for
`service.name`, then `resource`. tocsin always sets three attributes
itself:

- `service.name`: `tocsin`, unless one of the sources above names it.
- `service.version`: the running tocsin's version. It cannot be set.
- `service.instance.id`: the `node_id`, as on the `node` label. It cannot
  be set.

Header values are secrets: tocsin never logs them, nor a password in the
endpoint. Its startup line and failure warnings name the endpoint without
credentials and the headers by name only.

The table is the switch, with one exception: `OTEL_SDK_DISABLED=true`
(in any case) turns a configured export off, as the OpenTelemetry
specification has it. No other `OTEL_*` variable is then read, nothing
connects to a collector, and `tocsin serve` and `tocsin config check`
warn that the table is set but the export is off. Any other value leaves
the export on. `OTEL_METRICS_EXPORTER` is not read.

### Metrics

Each Prometheus family has an OTLP counterpart read from the same
counters at the same moment, so the two never disagree:

| OTLP metric | Type | Unit | Prometheus family |
|---|---|---|---|
| `tocsin.check.up` | gauge | `1` | `tocsin_check_up` |
| `tocsin.check.duration` | histogram, cumulative | `s` | `tocsin_check_duration_seconds` |
| `tocsin.check.last_run` | gauge | `s` | `tocsin_check_last_run_timestamp` |
| `tocsin.heartbeat.last_ping` | gauge | `s` | `tocsin_heartbeat_last_ping_timestamp` |
| `tocsin.check.results` | sum, cumulative, monotonic | `{result}` | none |
| `tocsin.component.status` | gauge | `1` | `tocsin_component_status` |
| `tocsin.slack.connections` | gauge | `{connection}` | `tocsin_slack_connections` |
| `tocsin.slack.connection.up` | gauge | `1` | `tocsin_slack_connection_up` |

`tocsin.check.results` counts the results each check and heartbeat has
recorded since tocsin started, one series per `tocsin.check.health`:
`up`, `degraded` or `down`. Unlike `tocsin.check.up` it counts raw
results, before confirmation. The notifier, reload, database and build
families are about tocsin itself and stay on `/metrics` only.

A metric is left out of an export while it has no points, which follows
the same rules as the absent series above. The two Slack metrics are read
from the database at each export, as `/metrics` reads them at each scrape,
and only when the Slack families would appear there.

The attributes carry the Prometheus labels under namespaced names:

| Attribute | Prometheus label | On |
|---|---|---|
| `tocsin.check.id` | `check` | every check metric |
| `tocsin.check.kind` | `kind` | every check metric |
| `tocsin.check.type` | none | checks: `http`, `tcp`, `dns`, `icmp` or `tls`; absent on heartbeats |
| `tocsin.component.id` | `component` | when the check has a component, and on `tocsin.component.status` |
| `tocsin.node.id` | `node` | every check and component metric |
| `tocsin.check.health` | none | `tocsin.check.results` |
| `tocsin.component.status` | `status` | `tocsin.component.status` |
| `tocsin.slack.purpose` | `purpose` | `tocsin.slack.connections` |
| `tocsin.slack.state` | `state` | `tocsin.slack.connections` |
| `tocsin.notifier.id` | `notifier` | `tocsin.slack.connection.up` |
| `tocsin.slack.connection` | `connection` | `tocsin.slack.connection.up` |

The OpenTelemetry semantic conventions have no names for any of these. A
check id is not `service.name` (it names a probe, not the service it
probes), a component is not a `service.namespace`, and the probed URL or
host is deliberately not sent, as it is not on `/metrics`. So every
attribute is in the `tocsin.` namespace, which the conventions reserve
for the application. `tocsin.node.id` repeats the resource's
`service.instance.id` on each point so that a backend which drops
resource attributes can still tell nodes apart.

### A Collector to receive it

An OpenTelemetry Collector that takes tocsin's metrics over either
protocol, writes them to Prometheus through remote write, and prints a
summary of each batch:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
      grpc:
        endpoint: 0.0.0.0:4317

processors:
  batch: {}

exporters:
  prometheusremotewrite:
    endpoint: http://prometheus:9090/api/v1/write
    resource_to_telemetry_conversion:
      enabled: true
  debug:
    verbosity: basic

service:
  pipelines:
    metrics:
      receivers: [otlp]
      processors: [batch]
      exporters: [prometheusremotewrite, debug]
```

`prometheusremotewrite` and `debug` are in the Collector's `contrib`
distribution. Through remote write the names arrive translated, such as
`tocsin_check_duration_seconds` and `tocsin_check_results_total`.

### gRPC

`protocol = "grpc"` needs a tocsin built with the `grpc` feature:

```sh
cargo install --locked --git https://github.com/closient/tocsin tocsin --features grpc
```

The release binaries are built without it, as it pulls in tonic and its
HTTP/2 stack. A tocsin without the feature refuses a `grpc` protocol at
startup and in `tocsin config check`, naming the feature, rather than
starting without the export. `http/protobuf` is always available and is
what most collectors and vendors accept.

### When an export fails

Each failure is logged as a warning, naming the endpoint (without
credentials) and why, and counted in
`tocsin_otlp_export_failures_total` on `/metrics`. An export fails when
the collector cannot be reached, answers with an error status, rejects
some of the points, or does not answer within the timeout. It also fails,
sending nothing, when the Slack connections cannot be read from the
database, as a scrape of `/metrics` then answers 500.

An attempt the collector refuses for now is retried, as the OTLP
specification has it: over `http/protobuf` an answer of 429, 502, 503 or
504; over `grpc` `UNAVAILABLE`, or `RESOURCE_EXHAUSTED` when the
collector sends a `RetryInfo`. So is an attempt that cannot reach the
collector: the connection refused, its name not resolved, the connection
not made within half the timeout, or the connection closed before the
collector answered (over `grpc`, tonic answers these `UNAVAILABLE`). The
retry waits as long as the collector's `Retry-After` header or
`RetryInfo` asks, or else 1 s, doubling with each retry of the same
export, jittered and never more than the interval. The same export is
sent again. A retry is made only if it would end, even waiting out the
timeout, before the next export is due, so retries never run into the
next interval: with the timeout equal to the interval (the default for
intervals of 10 s or less) there are none. Any other failure, an attempt
left unanswered for the whole timeout among them, is not retried. Every
failed attempt, retried or not, is counted and logged, the warning
saying whether it is retried and when, so an export that succeeds on its
second attempt still counts one failure. An export that is given up on
is not lost: the next one, an interval later, carries the cumulative
counts again, so a collector that recovers loses only the points in
between.

The export runs on its own task. It reads the counters the probes write,
and never waits on a probe, a database write or the page; the Slack
connections come from the database's readers. Reading them and the
first attempt share one timeout, so an export that is not retried ends
within it. A collector that hangs costs at most one timeout per interval
and slows nothing else.
On shutdown, on SIGTERM or SIGINT, tocsin sends one last export once
the checks have stopped, so the points since the last export are not
lost. An export under way or waiting to retry is abandoned for it; the
last carries the same counts and newer. It is one attempt, never
retried, and shutdown waits for it at most the timeout; if it fails it
is counted and logged like any other.

Alert on the counter alongside the rest:

```yaml
- alert: TocsinOtlpExportFailing
  expr: increase(tocsin_otlp_export_failures_total[15m]) > 0
  labels:
    severity: ticket
```

Only metrics are exported; traces are not.
