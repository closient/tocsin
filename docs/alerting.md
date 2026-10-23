# Alerting

tocsin tells its operators when a check or heartbeat fails, and tells them
again when it recovers. Every alert it sends has a matching resolve with the
same identity: a sender that only fires goes quiet on every later failure,
because the receiver never learns the first one ended.

The engine decides whether a change alerts (see [engine.md](engine.md):
`CheckChange::alerts()` and the outage's `alerted` flag, which already
accounts for maintenance windows). The `tocsin-alert` crate takes that
decision as given. It routes and delivers; it never looks at maintenance
windows itself.

## Identity

An alert is identified by the node, the subject's kind and the subject's
id: `node-a/check/api`, or `node-a/heartbeat/backup` for a heartbeat.
Configuration keeps a heartbeat from reusing a check's id, since both
record results under it. The fire, every repeat and the resolve carry the
same identity, so a receiver can pair them. A resolve also carries when the
alert started and ended, and how long it lasted.

One alert spans every outage from the change that fires it to the recovery
that ends it. A check that goes from degraded to down stays one alert; it
fires again with the new health, under the same identity.

## Routes

`[[route]]` sends alerts matching its filters to its notifiers (see
[config.md](config.md)). `checks` and `heartbeats` match the subject's id,
`components` and `groups` match its component, and `notify_on` is the least
severe health that alerts. An empty filter matches everything.

When several routes send one alert to the same notifier, it gets one copy,
repeated on the shortest of their `repeat` intervals. Every alert is
resolved: there is no way to turn a resolve off.

`repeat` (default `"4h"`, at least `"1m"`, or `"0s"` to never repeat) re-sends
an alert while it is still firing, titled `[STILL DOWN]` or
`[STILL DEGRADED]`. Repeats are not timers: each open alert stores when its
next repeat is due, so a restart neither loses nor duplicates one.

## Delivery

```
engine ── outage change ──▶ store transaction ──▶ alert_delivery (outbox)
                              (outage row +            │
                               outbox rows)            ▼
                                               delivery worker ──▶ notifiers
```

**The outbox.** The engine records each outage change through
`AlertingHistory`, which writes the outage row and the alert's outbox rows
in the same SQLite transaction (`Store::change_outage`). An outage that
alerted therefore always has its fires queued, and a recovery always has its
resolve queued, however far behind the worker is and whatever happens to
the process. The `EventBus` that feeds the live API is a lossy broadcast;
alerting does not use it. The tables are described in
[storage.md](storage.md#alert-and-alert_delivery).

**Ordering.** Each alert has two lanes on each notifier. Messages (fire,
repeat, resolve) go one at a time, oldest first, so a resolve is never sent
before its fire has been delivered, or given up on. A Pushover emergency
cancel is a delivery of its own in the second lane: it waits for every
message queued before it, so it never cancels a ring that has not started,
but no message ever waits for a cancel. A cancel that keeps failing is
retried and given up on like any delivery, and the resolve goes out on time
regardless.

**Retries.** A failed send is retried after a random wait of up to 5s,
then up to 10s, 20s and so on, capped at 10 minutes, for 12 attempts (up to
about an hour). The wait is drawn anew for each attempt (full jitter), so
alerts that failed together do not retry together. A send that runs past
30 seconds counts as failed. A 429 or 503 with a `Retry-After` header (in
seconds or as a date, at most an hour) is retried after that long instead,
and pauses the whole notifier until then: none of its other deliveries
start, because the service has said it is not taking any.

**Concurrency.** Each notifier sends at most `max_in_flight` deliveries at
once (default 2, set per `[[notifier]]`); the rest wait their turn. A storm,
such as every check failing when a network link drops, queues behind that
limit instead of tripping the service's rate limit. A permanent failure, such as an HTTP 4xx other
than 408 and 429, or a notifier that is no longer configured, is given up on
at once. A delivery given up on is kept as `failed`, with its last error,
for 90 days; delivered rows are kept for a week. Either way the next
delivery for that alert, such as its resolve, goes ahead.

**Independence.** Each delivery runs on its own task. A notifier that hangs
or fails holds up only its own alerts; the other notifiers' fires and
resolves go out on time. A send that panics is caught: it is logged as an
error naming the notifier, the alert and the delivery, counted as a failed
attempt, and retried like any other failure.

**Metrics.** Two counters, both labelled by notifier, are registered in the
process's Prometheus registry; the `/metrics` endpoint arrives separately.

- `tocsin_notifications_abandoned_total{notifier}` counts messages tocsin
  gave up on: a fire, repeat or resolve that was refused outright, or still
  failing after its last retry. Each one is a message an operator never got,
  so this is the counter to alert on, for example
  `increase(tocsin_notifications_abandoned_total[15m]) > 0`.
- `tocsin_notification_attempt_failures_total{notifier}` counts every failed
  attempt, retried or not. A notifier that is down shows a steady rate here
  long before its first message is abandoned; use it to see trouble coming,
  not to page.

A send still in flight when tocsin stops is neither: its row stays pending
and is sent after the restart.

**Shutdown and restart.** `serve` stops the engine first, so nothing is
queued after the worker goes, then stops the worker, then closes the store,
on every exit path. Sends in flight get two seconds to finish; any still
running are abandoned and their deliveries stay `pending`. Everything the
worker knows lives in the store, so a delivery pending at shutdown is sent
after the next start.

**Reloading.** A reload swaps the routes and notifiers at once; an alert
already firing keeps the notifiers it fired on. When a reload (or a start)
drops a check or heartbeat that has an outage open, the outage is
withdrawn: its alerts end without a resolve, since nobody saw it recover,
and one still ringing gets a cancel. When it drops a notifier, the alerts
open on it end and nothing more is sent through it; deliveries already
queued for it are abandoned and counted. See
[config.md](config.md#reloading).

**Secrets.** Webhook URLs, tokens and SMTP passwords never appear in logs or
in a recorded error: errors name the notifier, and a URL is shown without
its path or query.

**TLS.** Outbound HTTP uses one shared client with a 10-second timeout,
verifying against the system's root certificates. On a system without any,
`serve` keeps running: it logs an error at startup, and each HTTPS delivery
fails and is recorded as failed.

## Notifiers

| `type` | Fire and repeat | Resolve |
|--------|-----------------|---------|
| `slack`, `discord` | A message to the incoming webhook. | A message saying it recovered, and how long it lasted. |
| `telegram` | `sendMessage` to `chat_id`. | The same, saying it recovered. |
| `ntfy` | A POST to the topic with `Title`, `Priority` 4 when down (3 when degraded) and a tag. | Priority 3, tagged as resolved. |
| `pushover` | `down_priority` (default 1) or `degraded_priority` (default 0). Priority 2 is an emergency: Pushover rings it every 60s until acknowledged, for up to an hour. | `resolved_priority` (default 0). An emergency still ringing is cancelled, below. |
| `email` | One message per alert from `[server.email]`'s `from`, with its `reply_to`. | A message in the same thread. |
| `webhook` | Signed JSON, below. | The same JSON with `"event": "resolve"`. |
| `alertmanager` | `POST /api/v2/alerts`, re-posted every 60s with `endsAt` 5 minutes ahead. | `endsAt` set to when the outage ended. |

**Email.** `smtp_url` is `smtps://user:password@host:465` for TLS, or
`smtp://...:587`, which requires STARTTLS. `smtp://...?tls=none` sends in the
clear and is meant for a relay on the same host. Every message about one
alert carries the same `References` and an `X-Tocsin-Identity` header, so mail
clients thread the fire, its repeats and its resolve together; subjects start
`[DOWN]`, `[DEGRADED]`, `[STILL DOWN]` or `[RESOLVED]`. Neither `from` nor
`reply_to` may be a no-reply address (`noreply`, `no-reply`, `donotreply`
and the like, in any case): an operator must be able to answer an alert.

**Pushover emergencies.** A fire or repeat at priority 2 carries the tag
`tocsin-` and the hex SHA-256 of the user key, a zero byte and the alert's
identity: one tag per user and alert, revealing neither. When the alert
resolves, or fires again at a priority that does not ring (down to
degraded, say), tocsin queues a cancel, a `POST` to
`/1/receipts/cancel_by_tag/<tag>.json`, as its own delivery beside the
message. Whether a message rings is decided when it is queued, so changing
a priority in the config does not orphan a ring already going. A resolve
sent at priority 2 (`resolved_priority = 2`) is not tagged and rings until
acknowledged or it expires: pick a lower priority unless that is wanted.

**Alertmanager.** Alertmanager resolves a firing alert on its own once its
`endsAt` passes without a re-post. tocsin sets `endsAt` five minutes after
each post and re-posts every 60 seconds, whatever the route's `repeat`, so
the alert stays firing while tocsin is up, and resolves by itself within
five minutes if tocsin goes away. Alertmanager identifies an alert by its
whole label set, so the labels are the identity and nothing else:
`alertname` (`TocsinCheckFailing`, `TocsinHeartbeatFailing`, or
`TocsinTestAlert` for `tocsin notify test`), `node`, `subject_kind` and
`subject_id`. Everything that can change while an alert fires is an
annotation: `summary`, `description`, `health`, `name`, `component` and
`component_name` when there is one, and `tocsin_test` on a test alert. A
check that moves to another component, or from degraded to down, is
therefore still one alert in Alertmanager.

## The webhook

**Breaking change:** `signing_secret` is required. A `webhook` notifier
configured before signing existed fails `tocsin config check` until it has
one. Generate a secret with `openssl rand -hex 32`, put it in an environment
variable, set `signing_secret = "${ENV:NAME}"`, and have the receiver verify
each request as below. An unsigned webhook lets anyone who learns the URL
post fake alerts, so there is no way to opt out.

Each alert is a POST of this JSON:

```json
{
  "version": 1,
  "event": "resolve",
  "status": "resolved",
  "identity": "node-a/check/api",
  "node_id": "node-a",
  "subject": { "kind": "check", "id": "api", "name": "Public API" },
  "component_id": "web",
  "health": "down",
  "started_at_ms": 1800000000000,
  "ended_at_ms": 1800000090000,
  "duration_ms": 90000,
  "sent_at_ms": 1800000091000,
  "delivery_id": 42,
  "test": false,
  "title": "[RESOLVED] Public API (check api on node-a)",
  "text": "Public API recovered at ..."
}
```

`event` is `fire`, `repeat` or `resolve`; `status` is `firing` or
`resolved`. `ended_at_ms` and `duration_ms` are null until the resolve.
`delivery_id` is the same on every retry of one delivery, so a receiver can
drop duplicates. `version` changes only when a field changes meaning or goes
away.

### Verifying the signature

Every request carries:

| Header | Value |
|--------|-------|
| `X-Tocsin-Timestamp` | When it was signed, in unix seconds. |
| `X-Tocsin-Signature` | `sha256=` and the hex HMAC-SHA256, keyed with `signing_secret`, of the timestamp, a `.`, and the exact body bytes. |
| `X-Tocsin-Event` | `fire`, `repeat` or `resolve`. |
| `X-Tocsin-Delivery` | The delivery id. |
| `Authorization` | `Bearer <bearer_token>`, if one is configured. |

To verify, recompute the HMAC over the raw body as received, before parsing
it, compare in constant time, and reject a timestamp more than a few minutes
from your clock, so a captured request cannot be replayed later:

```python
import hashlib, hmac, time

def verify(secret: bytes, headers, body: bytes, tolerance=300) -> bool:
    timestamp = headers["X-Tocsin-Timestamp"]
    if abs(time.time() - int(timestamp)) > tolerance:
        return False
    expected = "sha256=" + hmac.new(
        secret, timestamp.encode() + b"." + body, hashlib.sha256
    ).hexdigest()
    return hmac.compare_digest(expected, headers["X-Tocsin-Signature"])
```

From a shell, with the body saved to `body.json`:

```sh
printf '%s.' "$TIMESTAMP" | cat - body.json \
  | openssl dgst -sha256 -hmac "$SIGNING_SECRET"
```

For example, the secret `secret`, timestamp `1800000000` and body `{"a":1}`
sign as
`sha256=440dfbb83aabca24d9e5027629cfd81cb45254690f8d410bb28f1cc17c990dbc`.

## Trying a notifier

```sh
tocsin notify test ops-slack --config <dir>
```

sends the notifier a test alert for the subject `tocsin-test`, then its
resolve, directly rather than through the outbox, and reports whether each
was accepted (exit 0 only if both were). Test alerts are titled `[TEST]`,
and carry `"test": true` in the webhook payload and `tocsin_test="true"` in
Alertmanager.
