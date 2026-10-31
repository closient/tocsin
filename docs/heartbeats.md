# Heartbeats

A heartbeat is a check that runs the other way round: instead of tocsin
probing a service, a scheduled job pings tocsin when it runs, and tocsin
marks the heartbeat down once a ping is overdue. Use one for anything that
has no endpoint to probe: a nightly backup, a cron job, a queue worker's
periodic sweep.

Apart from where its results come from, a heartbeat is a check. Each ping
and each missed deadline is a `check_result` row; it confirms, opens and
ends outages, alerts and resolves, sets its component's health, and shows
in status and history exactly as a probe does. The engine side is in
[`engine.md`](engine.md#heartbeats).

## Configuration

```toml
[[heartbeat]]
id = "nightly-backup"
name = "Nightly database backup"
component = "databases"
interval = "1d"
grace = "1h"

[[token]]
id = "backup-job"
secret = "${ENV:TOCSIN_BACKUP_TOKEN}"
scopes = ["heartbeat"]
```

`interval` is how often the job runs; `grace` (default one minute) is how
late a ping may be. Heartbeat and check ids share one namespace, since both
name rows in the same tables; loading refuses a heartbeat that reuses a
check's id. A heartbeat takes a [`class`](alerting.md#probe-classes) like a
check, so a drill's or a canary's alerts can be routed away from the pager.
The full reference is in [`config.md`](config.md#heartbeat).

`interval + grace` must be at most half of `[server] retention_days`
(90 days unless set), and `tocsin config check` refuses a heartbeat that
waits longer, pointing at its `interval` and at `retention_days`. A
heartbeat's deadline runs from its latest stored result, and a miss is
stamped with that deadline; if retention could delete the result first,
the miss would land in a day already purged, which the store refuses, and
the heartbeat could record nothing more. Half leaves room for retention's
whole-day boundaries and for a server that was down for a while.

## Tokens

A ping is a write, so it needs a `[[token]]` with the `heartbeat` scope.
There is no per-heartbeat secret: one token may ping every heartbeat, and
the token's `id` is what the server logs as the sender. Give each job, or
each machine running jobs, its own token so that one can be revoked
without the others. Secrets are compared in constant time.

## Pinging

```
POST /api/v1/heartbeat/{id}
GET  /api/v1/heartbeat/{id}
```

Both methods do the same thing; `GET` is there for clients that can only
fetch a URL. `HEAD` is refused with 405 and records nothing, so a link
checker or chat unfurler that previews a ping URL does not ping. Neither
`POST` nor `GET` takes a body. Everything else is in the query:

| Parameter | | |
|---|---|---|
| `status` | `up` or `down` | Default `up`. `down` says the job ran and failed. |
| `message` | string | Optional, at most 1024 bytes. Stored with the result; on a `down` ping it is the error shown. |
| `token` | string | The token, for clients that cannot send `Authorization: Bearer`. Percent-encoded; see below. |

| Status | When |
|---|---|
| 202 | Recorded. The body is `{id, status, received_at}`. |
| 401 | No token, or one that matches no `[[token]]`. |
| 403 | The token lacks the `heartbeat` scope. |
| 404 | No heartbeat has that id. |
| 405 | `HEAD`, or any method but `POST` and `GET`. |
| 422 | A `status` other than `up` or `down`, an unknown parameter, or a `message` over 1024 bytes. |
| 500 | The ping could not be stored. Nothing was recorded and the heartbeat's state and deadline are unchanged; try again. |
| 503 | The server is shutting down; try again. |

A 202 means the ping is stored, along with any outage it opens or ends.

The token is checked before the id, so without a valid token every id
answers 401 alike and nobody can learn which heartbeats exist. A holder of
a valid `heartbeat` token can tell them apart from the 404s; that is
accepted, since such a token can already ping every heartbeat.

`?token=` is accepted only on these routes, and the header wins when both
are sent. A ping URL that carries its token is a secret in its own right:
tocsin never logs request URLs, but a reverse proxy in front of it may, and
anything that fetches a URL it sees (a link checker, a chat unfurler, a
browser's prefetch) pings with `GET`. Prefer the header wherever the client
allows it, and never paste a tokened URL anywhere a bot reads.

The query is form-decoded: `%XX` is undone and a bare `+` reads as a
space. A secret containing `+`, `&`, `=`, `%` or a space must be sent
percent-encoded, which curl does with `-G --data-urlencode`:

```sh
curl -fsS -G --data-urlencode "token=$TOCSIN_TOKEN" \
  "https://status.example.com/api/v1/heartbeat/nightly-backup"
```

The ping is recorded with the server's clock, under the node that received
it: a heartbeat belongs to the node its pings reach.

### From the command line

```sh
export TOCSIN_URL=https://status.example.com
export TOCSIN_TOKEN=...   # the backup-job secret

if pg_dump ...; then
  tocsin heartbeat ping nightly-backup
else
  tocsin heartbeat ping nightly-backup --status down --message "pg_dump failed"
fi
```

Use `if`, not `pg_dump ... && ping || ping --status down`: in that form a
successful backup whose up ping fails (the server unreachable, say) runs
the down ping too, reporting a failure the job never had.

`tocsin heartbeat ping <id> [--status up|down] [--message TEXT]` takes the
token from `TOCSIN_TOKEN` (or `--token`, which shows in `ps`). It exits 0
once the server has recorded the ping, whichever status it carries; 2 when
the id is not a valid id (checked before any request) or the server
refuses the ping (401, 403, 404, 422); 3 when the server fails or cannot
be reached. `--json` prints the server's receipt.

From a shell without tocsin:

```sh
curl -fsS -X POST -H "Authorization: Bearer $TOCSIN_TOKEN" \
  "https://status.example.com/api/v1/heartbeat/nightly-backup"
```

## When a heartbeat is down

Every result, ping or miss, sets the next deadline to its own time plus
`interval + grace`. A ping is stamped with the time its request arrived. A
ping one millisecond before the deadline is on time; at the deadline, the
engine records a down result saying no ping arrived, stamped with the
deadline, and the next deadline runs from that miss. A job that has
stopped therefore records one miss per `interval + grace`, not one per
second. A ping that arrives at or after the deadline is late even if the
engine has not yet recorded the miss: it records the miss first, at the
deadline, then the ping.

If a miss cannot be stored, the engine logs an error and tries again after
a second, doubling up to a minute between tries, without moving the
deadline.

A heartbeat confirms on a single result: there is no `confirm` setting.

- A ping with `status=up`, on time or late, is up. A late ping after a
  miss recovers the heartbeat and ends its outage.
- A ping with `status=down` is down at once, without waiting for the
  deadline.
- A missed deadline is down.

## Node liveness

With `[federation]`, `tocsin serve` keeps a heartbeat for each node that
pushes to it, `node:<id>` ("Pushes from <id>"), and one for each node it
pushes to, `upstream:<id>` ("Pushes to <id>"). A push pings the first on
the node that takes it, once the push is committed, and the second on the
node that sent it, once the answer comes back. Each expects a ping every
`[federation] push_interval` and is down once `[federation] grace` passes
on top without one, so a node that stops pushing, or an upstream that
stops taking pushes, opens an outage that alerts like any other, and the
next push resolves it. A backlog sends pushes back to back; a ping is
recorded at most twice a `push_interval`.

A retired node, and a node whose `push_to` does not name this one, has no
`node:` heartbeat. Neither kind is in the configuration file, so `tocsin
config check` and `config show` do not list them; they are on the
internal page, never the public one, as they have no component. Their ids
hold a colon, which no configured id may, so the ping API answers 404 for
them. A route with no `checks`, `heartbeats`, `components` or `groups`
filter matches them, and a route's `heartbeats` can name them, to send a
node's outage to notifiers of its own:

```toml
[[route]]
id = "edge-pushes"
notifiers = ["oncall"]
heartbeats = ["node:edge-1", "upstream:hub"]
```

The node after the colon must be a `[[node]]` that is not retired, and
there must be a node that keeps the heartbeat: for `node:<id>`, `<id>`
pushes to at least one node; for `upstream:<id>`, some node pushes to
`<id>`. `tocsin config check` reports an unknown node, as for any other
reference. As every node reads the same files, such a route matches only
on the nodes that keep that heartbeat: `node:edge-1` on each node
`edge-1` pushes to, `upstream:hub` on each node that pushes to `hub`. A
silence can name one too, to quiet a node taken down on purpose.

## Uptime

A heartbeat's daily rollup weighs each result by how long it held, not by
how many there were: a job that pings sixty times in its first hour and
then misses every deadline for the rest of the day is about 4% up that
day, not 72%. See [`storage.md`](storage.md#daily_rollup).

## Restarts

The deadline is measured from the heartbeat's latest stored result, not
from when the server started, so a restart neither forgives a job that
has stopped nor punishes one that pinged just before it. A server that
was down past a heartbeat's deadline records one miss as soon as it
starts, stamped with the deadline it missed; the next deadline runs from
the start.

On start a heartbeat resumes the state it was in, as a check does: down if
its outage is still open, up if its last up result is within
`interval + grace`, and otherwise no state until its next result. Seeding
emits no events. A heartbeat that has never been pinged has no state, and
shows `unknown`, until its first ping or until `interval + grace` after the
server started, when it is missed.
