# Storage

Each node keeps its history in one SQLite file, `<data>/tocsin.db`, owned by
the `tocsin-store` crate. `tocsin serve` opens it at startup, switches it to
WAL, and runs any pending migrations before doing anything else.

## Conventions

- **Timestamps** are `INTEGER` unix milliseconds, UTC. Columns end in `_at`.
- **Days** are UTC. A rollup's `date` is `YYYY-MM-DD`; the day containing a
  timestamp is `started_at / 86400000`.
- **Enums** are `TEXT` with a `CHECK` listing every allowed value, so a bad
  value fails at the insert, not at the next read.
- **Tables are `STRICT`**: a value of the wrong type is rejected, not coerced.
- **JSON is never queried.** `check_result.detail` holds assertion detail for
  people to read; anything filtered, sorted or joined on gets its own column.

## Connections

| Setting | Value | Why |
|---|---|---|
| `journal_mode` | `WAL` | Readers never wait for the writer, and the writer never waits for readers. `Store::open` fails if WAL cannot be enabled (for example, on a network filesystem). |
| `synchronous` | `NORMAL` | Safe under WAL: a power cut can lose the last few commits but never corrupts the file. Probe results are cheap to lose; a full fsync per commit is not cheap. |
| `foreign_keys` | `ON` | An incident's components, updates and edits cascade with it. |
| `wal_autocheckpoint` | `0` on the writer | Commits never checkpoint; the checkpointer thread does (below). |
| `journal_size_limit` | 16 MiB on the writer | A log that a long read let grow is cut back to this once it rewinds. |
| `busy_timeout` | 5 s | Covers a checkpoint or a hand-run `sqlite3` holding a lock briefly. |

**One writer.** Every write goes through a single dedicated thread fed by a
channel. It drains whatever is queued (up to 256 writes) into one `IMMEDIATE`
transaction, gives each write its own savepoint so a bad row fails alone, and
answers each caller once the transaction commits. SQLite allows one writer
at a time anyway; owning that in one thread means no write ever spins on
`SQLITE_BUSY`, and batching turns 100 appends into one fsync-bounded commit.
If an error ends the batch's transaction instead of just its savepoint
(SQLite rolls the whole transaction back on some errors, such as
`SQLITE_FULL`), every write in the batch gets that error; none is reported
as committed. A write that panics is rolled back and answered with
`WriterPanicked`, the panic is logged, and the thread carries on with the
next write.

Every write transaction that reads before it writes is `IMMEDIATE`, taking
the write lock up front. A deferred one would read on a snapshot and then
need to upgrade it, which fails with `SQLITE_BUSY_SNAPSHOT` (no retry, no
busy wait) if another connection, such as a hand-run `sqlite3`, committed
in between.

**One checkpointer.** Under WAL, commits only append to the log; the
fsyncs happen when a checkpoint copies the log back into the database file.
SQLite's default runs that checkpoint inside whichever commit takes the log
past 1000 pages, on the committing connection, which here is the writer
thread: every write queued behind it waited for the copy and both fsyncs.
On CI runners those took 20 to 480 ms and were the whole p99 tail of the
write-latency bench (up to 320 ms; TOC-57). So the writer turns automatic
checkpoints off and, after each write, nudges a second thread with its own
connection, opened `query_only`, which runs `PRAGMA
wal_checkpoint(PASSIVE)` once 1000 frames (about 4 MB) are waiting, the
threshold SQLite's own automatic checkpoint uses, or a minute after the
first of fewer, so a quiet store still checkpoints. A WAL hook on the
writer counts the frames each commit appends. A passive checkpoint takes
no lock the writer or the readers need and skips frames a reader still
uses, so it never holds them up; one that a reader held back is tried
again a second later until it copies the whole log, and ten in a row
that leave 1000 frames or more behind log a warning.

**The log stays bounded.** A checkpoint copies only the frames committed
before it started, and SQLite rewinds the log to its start only when a
commit finds every frame copied. Under writes that never pause, every
checkpoint the checkpointer runs is raced by the next commit, so the log
would grow without bound. Once the log holds 4000 frames (about 16 MB),
the writer runs a passive checkpoint itself after its commit: nothing can
commit while it runs, so it copies the rest unless a reader holds frames,
and the next commit rewinds the log. Only one connection can checkpoint at
a time, and a passive checkpoint that finds another one running copies
nothing at all. The checkpointer's pass can take seconds on a disk busy
flushing other work, and writes that went on past it let the log reach
12005 frames (49 MB) on CI, then 16004 (TOC-59). So when the
checkpointer's pass holds the lock, the writer waits for it to end, on a
condition variable the pass signals, committing nothing meanwhile, and
then copies the rest; when another process holds it, the writer tries
again every 10 ms. While the writer checkpoints at the limit, and for a
second after, the checkpointer starts no pass of its own, which would
only take the lock again the moment the last one ended. The log then
holds at most 3999 frames plus those of the one commit that reached the
limit. A frame is a 4096-byte page and a 24-byte header, after a 32-byte
file header. A full batch of 256 check results commits 27 to 47 frames,
so under results alone the log stays within 4046 frames, 16,669,552
bytes; a bigger commit, such as a retention chunk, adds its own frames
once instead.

The bound has three exceptions. A long read can hold frames the
checkpoint may not copy: the writer does not wait for readers, since
`RESTART` or `TRUNCATE` would, but they wait while holding the write
lock, which puts reads back in front of writes; instead the writer logs
an error each time the log doubles past 8000 frames. The writer waits at
most 5 seconds (the connections' busy timeout), so a checkpoint that
does not finish shows up instead of stalling every write. And the writer's
checkpoint can fail. Either of the last two starts a lockout: the writer
logs one error, with SQLite's if there is one, and writes past the limit
try to checkpoint once each without waiting, until one succeeds. Then it
logs one line saying how long the lockout lasted and the most frames the
log held. A writer waiting out the checkpointer's pass holds every write
behind it for that long. Under a 60 GB file copy to the same disk, in
twelve runs of the bound's test, 31 of 36 writes at the limit waited,
for 0.57 seconds at the median and 4.9 at the most, and none timed out.
The longest pass took 5.9 seconds, longer than the writer waits, so a
pass like it can still start a lockout. The same load against a writer
that polled every 10 ms instead, beside a checkpointer that started its
next pass straight away, timed out four times, once after a 4.9-second
pass because the next pass had already started, and twice let the log
reach 11928 and 16001 frames behind a pass that outlasted the wait.

The disk is still shared: on ext4 a write can wait for the filesystem
journal while a checkpoint's fsync commits it. On a runner still flushing
a gigabyte of build output that cost single writes up to 200 ms, while the
p99 stayed under 12 ms. If the checkpointer thread ever dies, the writer
logs an error and turns automatic checkpoints back on rather than let the
log grow without bound. On close, and when the writer thread unwinds from
a panic, the writer's connection closes before it lets go of the
checkpointer, so the checkpointer's connection closes last, which
checkpoints and removes the log. A checkpointer panic fails `close` with
its own error.

**A pool of four readers**, each opened `query_only`, behind a semaphore.
Queries run on tokio's blocking pool. It is hand-rolled (about 140 lines)
rather than `r2d2`/`r2d2_sqlite` or `deadpool-sqlite`: those bring a
connection manager, health checks and extra dependencies for what is, with
a fixed pool of local file handles that never go stale, a `Vec` behind a
mutex. The borrowed connection travels with the query onto the blocking
thread and goes back when the query finishes, so a caller that stops waiting
(a timeout, a `select!`, an aborted task) cannot lose it. A query that
panics is answered with `ReaderPanicked` and its connection is replaced by a
fresh one; only if SQLite will not open another does the pool shrink, and a
pool with none left fails reads instead of hanging them.

## Tables

### `check_result`

One row per probe run. **Append-only**: the `check_result_append_only`
trigger aborts every `UPDATE`. `DELETE` stays open for retention. The
`check_result_not_before_retention` trigger aborts an insert dated before
`purged_before`, because that day's rollup is final.

| Column | Type | Notes |
|---|---|---|
| `id` | integer | `AUTOINCREMENT`, so an id is never reused, even after retention deletes the newest rows; retention relies on that. |
| `node_id` | text | Node that ran the probe. Non-empty. |
| `check_id` | text | Check from the config. Non-empty. |
| `started_at` | integer | When the probe started. At most an hour after the storing node's clock (`FUTURE_TOLERANCE`): a later one is refused with `FutureResult`, since it points at a clock that is wrong, not merely skewed. |
| `duration_ms` | integer | ≥ 0. |
| `health` | text | `up`, `degraded` or `down`. |
| `detail` | text | Valid JSON or null: per-assertion detail, for display only. |
| `error` | text | Transport or assertion error, if any. |
| `status_code` | integer | HTTP status, 0–999, or null. |
| `kind` | text | `check` (a probe's result) or `heartbeat` (a ping or a missed deadline). Added by 0006; older rows are `check`. |

Indexes: `(check_id, started_at)` for one check's history,
`(node_id, started_at)` for one node's results, a covering
`(started_at, check_id, node_id, health)` so rollups and retention
scan by time without touching the table, and the same columns over
heartbeat rows only (`WHERE kind = 'heartbeat'`, added by 0006). A rollup
weighs a heartbeat's results from that partial index and counts checks
from the full one, skipping any day a heartbeat has results on: `kind` in
the full index would widen the entry every result writes and every purge
deletes.

### `outage`

A machine-detected outage of one check on one node. The only change ever made
to a row is setting `ended_at`, once; the `outage_only_ends` trigger aborts any
other `UPDATE`. A partial unique index (`outage_open`) allows at most one open
outage (`ended_at IS NULL`) per check per node. Outages are kept indefinitely:
they are small and they are the incident history.

| Column | Type | Notes |
|---|---|---|
| `id` | integer | Row id. |
| `node_id`, `check_id` | text | Non-empty. |
| `component_id` | text | Status-page component, or null. |
| `health` | text | `degraded` or `down`. |
| `started_at` | integer | |
| `ended_at` | integer | Null while open; ≥ `started_at`. |
| `alerted` | integer | 1 if the change that opened it alerted, 0 if a maintenance window silenced it; its recovery alerts the same way. Added by 0003; older rows are 1. |

### `daily_rollup`

Per UTC day figures from `check_result`, one row per check, node and day,
kept forever. The 90-day status bar is drawn from here, never from raw
results. `total = up + degraded + down` is enforced by a `CHECK`; `worst` is
the worst health seen that day.

A check's figures are counts of results. A heartbeat's are milliseconds:
each result holds its health until the next result that day, the end of the
day, or now, whichever comes first, and time before the day's first result
is not counted. A heartbeat reports only when its job runs or a deadline
passes, so counting would weigh an hour of pings every minute against a day
of hourly misses: sixty pings in one hour followed by 23 hourly misses
count as 72% up, but held 4% of the day.

| Column | Type | Notes |
|---|---|---|
| `check_id`, `node_id` | text | Primary key with `date`. |
| `date` | text | `YYYY-MM-DD`, UTC; a `CHECK` rejects anything `date()` would not round-trip. |
| `total`, `up`, `degraded`, `down` | integer | Counts for a check, milliseconds for a heartbeat. |
| `worst` | text | `up`, `degraded` or `down`. |
| `kind` | text | `check` or `heartbeat`: which of the two the figures are. Added by 0006; older rows are `check`. |

### `retention`

A single row (`id = 1`) holding `purged_before`: a UTC day boundary. Raw
results before it have been rolled up and are deleted, or being deleted.
Rollups for days before it are **final**: nothing recomputes them, because
the raw rows they came from are gone, and no result for them can be stored.

### Heartbeat results

Heartbeats have no table of their own. Each ping, and each deadline the
engine records as missed, is a `check_result` row under the heartbeat's id,
with `kind` `heartbeat` and `duration_ms` 0; a ping's `message` is in
`detail` as `{"message": ...}` and, on a down ping, in `error` too. Outages
and retention treat them like any check's; rollups weigh them by time (see
`daily_rollup`). A result and the outage change it causes are stored in one
transaction, with the alert deliveries the change queues, so a ping is never
acknowledged without all of them. Migration
`0006_heartbeats` drops the `heartbeat_ping` table that `0001` reserved
for them, unused until then.

### `incident`, `incident_component`, `incident_update` and `incident_edit`

Migration 0002 replaced the placeholder incident tables 0001 created (no
code path ever wrote them). The API that writes these tables is in
[`incidents.md`](incidents.md).

`incident` is one incident or scheduled maintenance: `kind` (`incident` or
`maintenance`), `title`, `impact` (`none`, `minor`, `major`, `critical`),
`status`, `scheduled_start` and `scheduled_end` (maintenance only, end after
start), `resolved_at`, and `created_at`/`created_by` and
`updated_at`/`updated_by`. Each `*_by` column has a `*_by_kind` beside it:
`token` (a configured `[[token]]`, by id) or `person` (reserved for a name an
authenticating proxy supplies). The id is `AUTOINCREMENT`, so it is never
reused and links to an incident stay true. `CHECK`s hold each kind to its own
statuses (`investigating`, `identified`, `monitoring`, `resolved` for an
incident; `scheduled`, `in_progress`, `completed` for maintenance), keep a
schedule on maintenance and off incidents, and set `resolved_at` exactly
when the status is `resolved` or `completed`. A trigger fixes the id, kind
and creation fields.

**An edit (`PATCH`) changes the `incident` row in place** (title, impact,
schedule, and the component set in `incident_component`), moves
`updated_at`/`updated_by`, and appends one `incident_edit` row holding the
fields it set as JSON with who sent it. The JSON is for people reading the
audit trail; the `incident` row is the source of truth.

**Updates are append-only.** Each `incident_update` row is one entry on the
timeline: `status`, a Markdown `message`, `created_at` and its author. The
first is the opening message. Posting one moves the incident's `status` to
it; `resolved_at` keeps the first resolution and clears if an update
reopens the incident. Triggers refuse an `UPDATE` of `incident_update` or
`incident_edit`. All four tables cascade with their incident.

### `release`

One release of a `[[project]]` to one environment, pushed by CI
([releases.md](releases.md)). Migration 0004 replaced 0001's placeholder,
which nothing ever wrote. Columns: `project_id`, `version`, `environment`,
`title`, Markdown `notes`, an optional `url`, `released_at`, `deployed_at`,
and `created_*` / `updated_*` with who wrote each, like `incident`.

**`(project_id, version, environment)` is unique**, and every write is one
`INSERT … ON CONFLICT DO UPDATE`: a retried publish or deploy updates the
row instead of adding one. A deploy stamped before its publish creates the
row with an empty title and notes and a NULL `released_at` (a *stub*); the
publish fills it in. `CHECK`s hold the version and environment to
`A-Za-z0-9._+-` starting with a letter or digit (64 and 32 characters),
titles to 200 characters, notes to 64 KiB, a row to being published or
deployed, and a published row to having a title. Triggers refuse changing
a row's id, key or creation, and moving `released_at` earlier or back to
NULL. Indexed by `(project_id, released_at)`, by `released_at`, and by
`coalesce(deployed_at, released_at)`, where the timeline places it. 0004
also adds `outage_by_start` on `outage (started_at)` and
`incident_by_created` on `incident (created_at)`, so the timeline reads
every kind of event from an index.

### `alert` and `alert_delivery`

The alerting outbox, added by 0005 (see [alerting.md](alerting.md)). Rows are
written in the same transaction as the outage change that causes them, so an
outage that alerted always has its deliveries queued and a recovery always
has its resolve queued, across restarts and however far the delivery worker
is behind.

An `alert` is one notifier's view of one alert identity, from the change that
fires it to the recovery that ends it, across every outage in between. A
partial unique index (`alert_open`) allows at most one open alert
(`ended_at IS NULL`) per node, subject and notifier.

| Column | Type | Notes |
|---|---|---|
| `id` | integer | Row id. |
| `node_id`, `subject_id` | text | The identity, with `subject_kind`. |
| `subject_kind` | text | `check` or `heartbeat`. |
| `component_id` | text | Or null. |
| `notifier_id`, `route_id` | text | Where it goes, and the first route that sent it there. |
| `outage_id` | integer | The outage it fired for most recently. |
| `health` | text | `degraded` or `down`: its latest. |
| `started_at`, `ended_at` | integer | `ended_at` is null while it fires. |
| `resolve` | integer | 1 if its end sends a resolve. |
| `repeat_ms`, `next_repeat_at` | integer | The repeat interval (null never repeats) and when the next is due. |

An `alert_delivery` is one message to send: `kind` (`fire`, `repeat` or
`resolve`), the `health` it reports, `status` (`pending`, `delivered` or
`failed`), `attempts`, `next_attempt_at`, `last_error` (redacted) and
`finished_at` (null exactly while pending). The worker is only offered an
alert's oldest pending delivery, which is what keeps a resolve behind its
fire. Delivered rows are pruned after a week and failed ones after 90 days;
an `alert` whose deliveries are all gone and which has ended goes with them.

## Rollups

Rollups are recomputed from raw results, never incremented, so recomputing
is idempotent: run it twice and the table is the same.

- **Hourly**, `serve` recomputes from the latest day already rolled up (it may
  have been partial) to now, but never starting later than yesterday:
  normally yesterday and today. The cap keeps a rollup for a future day
  (left by a node whose clock ran ahead) from freezing today's.
- **On demand**, `tocsin db rollup --data <dir> --since YYYY-MM-DD` recomputes
  every day from `--since` on. Days before `purged_before` are skipped, and the
  output's `since` says where it really started.

The aggregate runs on a reader connection; only the upsert goes through the
writer, so a long recompute never holds the write lock while it scans.

## Retention

`retention_days = N` in `[server]` (default 90, at least 1; see
[config.md](config.md)) keeps N whole UTC days of raw results before today.
Each hour, after rolling up:

1. Finalize the expiring days one at a time, oldest first. A day is
   aggregated on a reader, since it can be hundreds of thousands of rows and
   scanning them must not hold the write lock. Then one `IMMEDIATE` write
   transaction upserts that day's rollups and raises `purged_before` past
   it. A result committed between the two would otherwise be deleted
   without being counted, so the write transaction first looks for any row
   in the day with an id above the highest the read saw, and re-aggregates
   under the lock if it finds one. Once the watermark is raised, the trigger
   on `check_result` refuses results for that day. The status bar never
   loses a day, even if the process dies between days.
2. Delete results older than `purged_before` in chunks (2 000 rows by
   default), each its own short write transaction, pausing between chunks.
   Writes queued meanwhile commit between chunks; readers never wait under
   WAL. A pass that stops part-way is finished by the next one.

Deleted pages are reused by later inserts, so the file stops growing once the
window is full. `tocsin db vacuum --data <dir>` rebuilds the file to give the
space back to the filesystem. It holds the write lock for the whole rebuild,
so it refuses to run (`invalid_input`, exit 2) while `serve` is running on
the same folder.

## Inspecting

```sh
tocsin db info --data <dir> --json     # size, schema version, row counts, oldest/newest result
tocsin db rollup --data <dir> --since 2026-10-01 --json
tocsin db vacuum --data <dir> --json
```

None of them create a database; pointing one at a folder without `tocsin.db`
is `invalid_input` (exit 2).

`db info` opens the file read-only and never migrates it, so it can describe
a database a newer build wrote: `schema_version` is the file's, and
`supported_schema_version` is this binary's. `db rollup` and `db vacuum`
open the store like `serve` does, migrating an older file and refusing a
newer one.

**The data folder's lock.** `serve` holds an exclusive lock (`flock`) on
`<data>/tocsin.lock` while it runs; the operating system drops it when the
process ends, however it ends. A second `serve` on the same folder, and
`db vacuum`, are refused while it is held. `db info` and `db rollup` do not
take it: info only reads, and rollup's upserts queue for the write lock like
any other write.

## Migrations

Migrations live in `crates/tocsin-store/migrations/`, numbered, and are applied
in order by `rusqlite_migration`, which records progress in
`PRAGMA user_version`. **A shipped migration is never edited**: change the
schema by adding the next file. A database whose `user_version` is newer than
the binary knows is refused rather than guessed at; only `db info`, which
never writes, will describe one.
