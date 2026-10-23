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

Indexes: `(check_id, started_at)` for one check's history,
`(node_id, started_at)` for one node's results, and a covering
`(started_at, check_id, node_id, health)` so rollups and retention scan by
time without touching the table.

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

### `daily_rollup`

Per UTC day counts of `check_result`, one row per check, node and day, kept
forever. The 90-day status bar is drawn from here, never from raw results.
`total = up + degraded + down` is enforced by a `CHECK`; `worst` is the worst
health seen that day.

| Column | Type | Notes |
|---|---|---|
| `check_id`, `node_id` | text | Primary key with `date`. |
| `date` | text | `YYYY-MM-DD`, UTC; a `CHECK` rejects anything `date()` would not round-trip. |
| `total`, `up`, `degraded`, `down` | integer | Counts. |
| `worst` | text | `up`, `degraded` or `down`. |

### `retention`

A single row (`id = 1`) holding `purged_before`: a UTC day boundary. Raw
results before it have been rolled up and are deleted, or being deleted.
Rollups for days before it are **final**: nothing recomputes them, because
the raw rows they came from are gone, and no result for them can be stored.

### `heartbeat_ping`

A push heartbeat received for a check: `node_id`, `check_id`, `received_at`,
and the `source` address. Append-only (trigger `heartbeat_ping_append_only`).
Indexed by `(check_id, received_at)`. Its shape belongs to the heartbeat
work, which extends it with a new migration.

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

A release or deploy pushed by CI: `version`, optional `component_id` and
`environment`, Markdown `notes`, `deployed_at` (when it shipped) and
`received_at` (when tocsin heard about it). Indexed by `deployed_at`.

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
