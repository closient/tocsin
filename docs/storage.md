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
| `journal_size_limit` | 32 MiB on the writer | A log that a long read let grow is cut back to this once it rewinds. |
| `busy_timeout` | 5 s | Covers a checkpoint or a hand-run `sqlite3` holding a lock briefly. |

**One writer.** Every write goes through a single dedicated thread fed by a
channel of 1024. It drains whatever is queued (up to 256 writes) into one
`IMMEDIATE` transaction, gives each write its own savepoint so a bad row fails
alone, and answers each caller once the transaction commits. One write is
answered sooner: a check's probe result that opens or ends no outage is
answered once the channel takes it, so a slow commit never holds up a
probe (DESIGN.md, "Durability", has the contract and what a crash loses).
Every other write, including every heartbeat ping and every result that
changes a check's state, is still answered only once committed. A full
channel makes every caller wait, early-answered or not. A result the
database refuses after it was answered is logged at `error` and counted in
`CheckpointStats::rejected`. SQLite allows one writer
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
uses, so it never blocks them on a lock. That is all the separate thread
guarantees: it takes the copy and its fsyncs out of the writer's commits,
not off the disk the writer commits to, and a write can still stall while
a pass runs (see below). One that a reader held back is tried
again a second later until it copies the whole log, and ten in a row
that leave 1000 frames or more behind log a warning.

**The log stays bounded.** A checkpoint copies only the frames committed
before it started, and SQLite rewinds the log to its start only when a
commit finds every frame copied. Under writes that never pause, a
checkpoint the checkpointer runs is raced by the next commit, so the log
would grow without bound. Past a soft limit of 4000 frames (about 16 MB),
every write wakes the checkpointer at once, and a pass that copied all it
found is followed straight away by one for the frames committed during
it; the first that no commit races leaves the whole log copied, and the
next commit rewinds it. The writer itself does no checkpoint work below
the hard limit of 8000 frames (about 32 MB): a checkpoint on the writer
thread, however few frames it copies, fsyncs the log and the database
inside the write, and under a disk busy with other work those fsyncs were
the bench's whole tail (TOC-60). Once the log holds 8000 frames, the
writer runs a passive checkpoint itself after its commit: nothing can
commit while it runs, so it copies the rest unless a reader holds frames,
and the next commit rewinds the log. Only one connection can checkpoint at
a time, and a passive checkpoint that finds another one running copies
nothing at all. The checkpointer's pass can take seconds on a disk busy
flushing other work, and when the limit was 4000 frames, writes that went
on past it let the log reach 12005 frames (49 MB) on CI, then 16004
(TOC-59). So when the
checkpointer's pass holds the lock, the writer waits for it to end, on a
condition variable the pass signals, committing nothing meanwhile, and
then copies the rest; when another process holds it, the writer tries
again every 10 ms. While the writer checkpoints at the hard limit, and
for a second after, the checkpointer starts no pass of its own, which
would only take the lock again the moment the last one ended. The log
then holds at most 7999 frames plus those of the one commit that reached
the hard limit. A frame is a 4096-byte page and a 24-byte header, after a
32-byte file header. A full batch of 256 check results commits 27 to 47
frames, so under results alone the log stays within 8046 frames,
33,149,552 bytes; a bigger commit, such as a retention chunk, adds its
own frames once instead.

The bound has three exceptions. A long read can hold frames the
checkpoint may not copy: the writer does not wait for readers, since
`RESTART` or `TRUNCATE` would, but they wait while holding the write
lock, which puts reads back in front of writes; instead the writer logs
an error each time the log doubles past 16000 frames. The writer waits at
most 5 seconds (the connections' busy timeout), so a checkpoint that
does not finish shows up instead of stalling every write. And the writer's
checkpoint can fail. Either of the last two starts a lockout: the writer
logs one error, with SQLite's if there is one, and writes past the hard
limit try to checkpoint once each without waiting, until one succeeds. Then it
logs one line saying how long the lockout lasted and the most frames the
log held. A writer waiting out the checkpointer's pass holds every write
behind it for that long. With TOC-59's single limit of 4000 frames, under
a 60 GB file copy to the same disk, in twelve runs of the bound's test, 31 of 36 writes at the limit waited,
for 0.57 seconds at the median and 4.9 at the most, and none timed out.
The longest pass took 5.9 seconds, longer than the writer waits, so a
pass like it can still start a lockout. The same load against a writer
that polled every 10 ms instead, beside a checkpointer that started its
next pass straight away, timed out four times, once after a 4.9-second
pass because the next pass had already started, and twice let the log
reach 11928 and 16001 frames behind a pass that outlasted the wait.

With the two limits (TOC-60), in the seven of ten runs each of the
write-latency bench that nothing else on the disk slowed past 100 ms,
writes reached TOC-59's single limit 88 times and the hard limit 3 times,
and the writer's copies there stalled writes for 318 ms and 8 ms in all;
the p99 ranged from 7.8 to 28.4 ms before and from 6.8 to 21.3 ms after.
Under a disk kept busy by other writes, passes take seconds, the
checkpointer falls behind, and the writer still reaches the hard limit,
though less often: 17 times against 117 in ten runs beside a loop of 128
MB synced writes. Most of the stall left there is the writer's own commits
waiting on the disk, not checkpoint work. In twelve runs of the bound's
test beside a 60 GB file copy, 21 of 43 writes at the hard limit waited,
for 0.4 ms at the median; one waited the whole 5 seconds behind an
11-second pass and started a lockout.

**Retention waits for the checkpointer.** Retention's purge is the one
writer that outruns the checkpointer: each chunk of 2000 rows appends
hundreds of frames, chunks follow each other 10 ms apart, and the passes
chasing them each copy the 16 MB above the soft limit. In the bench the
purge carried the log to the hard limit, and the check result that landed
there waited out the writer's copy: on CI the log reached 8262 frames
during the purge, and once 32.1 MB with a 576 ms worst write (TOC-62). So
before each of its writes, the rollup and every delete chunk, retention
checks the log: past the soft limit, with frames that no complete pass has
copied, it waits for the checkpointer, checking every 10 ms, for at most
the same 5 seconds. A pass that copied everything lets retention's next
commit rewind the log, so the purge runs at the speed the disk copies it,
and its frames are paid for by the purge rather than by a check result
queued behind it. A pass a reader held back is tried again a second
later, so a chunk can wait that long. A wait that times out writes
anyway and logs a warning with the frames in the log and how long it
waited; the hard limit still bounds the log. Retention waits between its
writes, never inside one: it holds no transaction and no place in the
writer's queue while it waits, so check results keep committing.

`Store::checkpoint_stats` counts every path above: writes at the hard
limit, how long they waited and copied, lockouts, stand-asides, passes
(longest and p99), chase passes, rewinds, the longest write, and
retention's waits, and keeps each write, pass or wait over 10 ms as a
timed event. It also measures commit lag, how long each insert waited from
entering the queue to its commit (p99, longest, and the commits whose oldest
insert waited over 50 ms, each kept as a timed event), and counts results
refused after they were answered. The write-latency bench prints them
beside its latency table, which shows three series: a probe result as its
caller saw it (the budget), a state change waiting for its commit, and the
commit lag of every insert. It lists every stall over budget and the events
that overlapped it, so a failing run names its path, and a slow disk shows
up in the commit lag even when no caller waited for it.

On Linux, `scripts/bench/fsync_shim.c` is an `LD_PRELOAD` shim over `fsync`
and `fdatasync`. With `TOCSIN_SYNCTRACE=1` it prints every sync with its
thread (`tocsin-store-wr` for the writer, `tocsin-store-ch` for the
checkpointer), file, start and duration; with `TOCSIN_FSYNC_DELAY_MS` it
slows syncs of files ending in `TOCSIN_FSYNC_DELAY_SUFFIX` (default
`-wal`). CI runs the bench under the tracer, and `scripts/bench/sweep.py`
reads its log and names, for every append over 50 ms, the rewinds and log
syncs it overlapped. `tests/slow_fsync.rs` uses the delay to show a 300 ms
header sync no longer reaches a probe's caller.

Writes still stall below both limits, while a pass is running. On CI the
bench recorded a write of one check result taking 93 ms with the log far
under the soft limit, entirely inside a 222 ms pass:

```text
- 16.048–16.141 s write 92.76ms, log at 2079 frames after
- 15.919–16.141 s checkpointer pass 222.30ms
```

Locally, on a shared rotational disk beside a loop of synced writes,
commits of a single row took up to 1.38 s inside a 1.42 s pass. The
working hypothesis is that the two connections are coupled through the
filesystem rather than through SQLite: the writer commits with
`synchronous = NORMAL`, which does not sync, and the passive pass takes no
lock it needs, but on ext4 a write can wait for the filesystem journal
while the pass's fsync commits it. Nothing here has measured that wait
directly; the evidence is only that the stalls fall inside passes and
that no SQLite lock explains them. Durability is not lowered to chase
them. If they break the latency budget, the lever is shorter passes (fewer
frames copied per pass), measured first. If the checkpointer thread ever dies, the writer
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
| `origin_store`, `origin_id` | integer | For a result another node pushed: its store's `node_store` row and the result's id there, unique together (`check_result_origin`) so a batch applied twice stores it once. Both null on local rows, never one alone. Added by 0019. |

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
to a row is setting `ended_at`, `ending` and `ended_seq` together, once; the
`outage_only_ends` trigger aborts any other `UPDATE`, and `outage_numbered`
aborts an insert without `opened_seq`, or with `ended_seq` set exactly when
`ended_at` is not. A partial unique index (`outage_open`) allows at most one open
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
| `ending` | text | Null while open, else `recovered` (tocsin saw the check leave this health) or `withdrawn` (the check was removed from the configuration while out). Added by 0008, which set every outage already ended to `recovered`. |
| `alerted` | integer | 1 if the change that opened it alerted, 0 if a maintenance window silenced it; its recovery alerts the same way. Added by 0003; older rows are 1. |
| `opened_seq`, `ended_seq` | integer | Where its store opened and ended it in one sequence that numbers both (`node_store.next_seq` for this store), so another store replaying those changes in seq order never has two open for one check. `ended_seq` is null while open and greater than `opened_seq`. Added by 0019, which numbered existing outages in id order, each end straight after its open. |
| `origin_store`, `origin_id` | integer | For an outage another node pushed: as on `check_result` (`outage_origin`). Added by 0019. |

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

**Drafts** (migration 0011): `published_at`, `published_by_kind` and
`published_by` are all `NULL` on a [draft](incidents.md#drafts) and all set
once it is published; a `CHECK` keeps them together. The migration
backfilled every existing incident as published by its creator at its
creation, so none became a draft. A trigger refuses changing them once
set, so an incident is published once and never goes back to being a
draft. Every public read filters on `published_at IS NOT NULL`; a partial
index on the drafts serves the internal board's list. Discarding a draft
deletes its row, and its updates and edits cascade with it; a published
incident is never deleted.

**Internals** (migration 0012): `internal_note` and `external_key`, both
optional, never an empty string. The note is Markdown for the people
working the incident; an edit replaces it. The key is set at creation only
(a trigger refuses changing it). Since migration 0013 a partial unique
index allows each non-`NULL` key once among incidents with `resolved_at
IS NULL`, so a create carrying a key an open incident already has answers
that incident instead of inserting. Resolving an incident (or completing
a maintenance) sets `resolved_at` and frees its key; discarding a draft
deletes the row and frees it too. A second, non-unique partial index on
the key serves the list of every incident with a key. Reopening clears
`resolved_at`, so the store refuses a reopen while another open incident
holds the key, and the unique index backs that up.
Ordinary reads leave both columns out; one read fetches them by incident
id for the callers trusted with them (see
[incidents.md](incidents.md#internal-note-and-external-key)). An edit's
`incident_edit` row records a new note like any other field.

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
(`ended_at IS NULL`) per node, subject and notifier. `alert_by_start`, on
`(started_at, id)`, serves the [alert history](api.md) newest first
(added by 0017).

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
| `notified` | integer | 1 once a fire or repeat was delivered for the alert's current fire; a re-fire at a new outage or health resets it to 0. While it is 0 and the latest fire or repeat was held, a lapsed [silence](alerting.md#silences) queues a fresh fire. Added by 0014; older rows are 1. |
| `attempted` | integer | 1 once a fire or repeat send was started, set before the request goes out, whatever its outcome; never less than `notified`. While it is 0 a silence holds back the resolve and a pending cancel is deleted. Added by 0014; older rows are 1. |
| `labels`, `annotations` | text | JSON objects of strings: the subject's configured labels and annotations when the alert opened. Every message of the alert sends these, so a reload never changes its Alertmanager identity mid-episode. Added by 0015; older rows are `{}`. |
| `ended_by` | text | Null while it fires, else `recovery` (its outage recovered) or `config` (its check, heartbeat or notifier was removed from the configuration); set with `ended_at`. Its resolve says which. Added by 0016; alerts that ended before it are null and read as `recovery`. |

An `alert_delivery` is one message to send: `kind` (`fire`, `repeat`,
`resolve` or `cancel`), the `health` it reports, `status` (`pending`,
`delivered`, `failed` or `silenced`), `attempts`, `next_attempt_at`,
`last_error` (redacted), `finished_at` (null exactly while pending) and
`silence_id` (set exactly when `silenced`: the silence that held it back).
The worker is only offered an alert's oldest pending delivery, which is
what keeps a resolve behind its fire. Delivered rows are pruned after a
week, and failed and silenced ones after 90 days; an `alert` whose
deliveries are all gone and which has ended goes with them.

### `silence`

[Silences](alerting.md#silences), added by 0014. History, not
configuration: a row is never deleted, and never changed except to set
`ended_at`, `ended_by_kind` and `ended_by`, once (a trigger refuses
anything else).

| Column | Type | Notes |
|---|---|---|
| `id` | integer | Row id. |
| `node_id`, `subject_kind`, `subject_id`, `component_id`, `notifier_id` | text | The matchers; null matches anything. At least one, or `episode`, is set. |
| `episode` | integer | The `outage` it is scoped to, or null; it stops applying when that outage ends, and holds only alerts whose `outage_id` it is. A `CHECK` requires `node_id` and `subject_id` with it. |
| `reason` | text | Not empty. |
| `created_at`, `created_by_kind`, `created_by` | | When, and the token or person the server authenticated. |
| `expires_at` | integer | Required, after `created_at`; never moved. A request's `expires_in` is stored as the instant it gives. |
| `ended_at`, `ended_by_kind`, `ended_by` | | Set together when someone ends it early. |

A silence applies while `ended_at` is null, `expires_at` is ahead, and its
episode, if any, is still open.

### `subscriber`, `subscription`, `subscriber_event` and `subscriber_delivery`

Email subscribers to the public page, added by 0009 (see
[subscribers.md](subscribers.md)).

A `subscriber` is one address, lowercased and unique, with `created_at`,
`confirmed_at` (null until confirmed) and `manage_secret`, the random half
of its manage and unsubscribe links. Until it is confirmed it has a
`confirm_hash`, the SHA-256 of its confirmation token, so the database
never holds a working link, and `confirm_expires_at`. `CHECK`s keep the
two null together, and null exactly when the address is confirmed.
Unconfirmed rows past their expiry are deleted each hour.

A `subscription` is one `(subscriber_id, kind, target)`: `kind` is
`component` or `project`, and `target` an id or `*`.

A `subscriber_event` is one announcement: its unique `key`
(`incident:7`, `incident:7:update:12`, `release:api:1.4.0`), the `scope`
it matches (`component` or `project`), its rendered `subject` and `body`,
`created_at`, and `expanded_at`, set when its deliveries are queued.
`subscriber_event_target` lists the components or project it is about.
It is written in the same transaction as the incident, update or release
it announces, and kept after its deliveries are pruned, so its key is
never queued twice.

A `subscriber_delivery` is one `(subscriber_id, event_id)`, unique: the
delivery log. `status` is `pending`, `sending`, `sent` or `failed`, with
`attempts`, `next_attempt_at`, `last_error` and `finished_at` (null exactly
while `pending` or `sending`). A delivery is set to `sending` in its own
committed transaction before the SMTP server sees it, and one found
`sending` at startup is failed, never resent. Finished rows are deleted
after 30 days. Deleting a subscriber deletes its subscriptions and every
delivery still queued for it.

### `slack_connection`, `slack_subscription` and `slack_delivery`

Slack channels connected by signing in to Slack, added by 0010 (see
[alerting.md](alerting.md#slack) and [subscribers.md](subscribers.md#slack)).

A `slack_connection` is one incoming webhook Slack handed over.
`purpose` is `operator`, for a channel notifiers name with `name`
(unique), or `subscriber`, for a channel a visitor added, with
`manage_secret`, the random half of its manage and remove links. `CHECK`s
tie `name` to `operator` and `manage_secret` to `subscriber`.
`webhook_url` is a secret: anyone holding it can post to the channel, so
it is read only to post and never leaves the store in a page, an API
answer or a log. `channel`, `channel_id`, `team`, `team_id` and
`configuration_url` are what Slack said about it, with `created_by` (the
admin, for an operator connection) and `created_at`. `dead_at` and
`dead_reason` are set together when Slack answers that the webhook is
gone; connecting the same name, or the same subscriber channel, again
replaces the row and clears them.

A `slack_subscription` is one `(connection_id, kind, target)`, as
`subscription` is for an address.

A `slack_delivery` is one `(connection_id, event_id)`, unique, with the
same `status`, `attempts`, `next_attempt_at`, `last_error` and
`finished_at` as `subscriber_delivery`, and the same rules: `sending` is
committed before the post, a delivery found `sending` at startup is
failed, and finished rows are deleted after 30 days. Marking a connection
dead fails every delivery still queued for it in the same transaction.
Deleting a connection deletes its subscriptions and deliveries.

### `node_store`

Which store this is, and the stores of other nodes it has heard from, added
by 0018. The migration writes one row with `local = 1` and a random
`store_id` (32 lowercase hex digits), so every database has an identity of
its own that survives restarts and renames, and a fresh database is a new
one; a partial unique index (`node_store_local`) allows only one such row.

| Column | Type | Notes |
|---|---|---|
| `id` | integer | Row id. |
| `node_id` | text | The node a remote store belongs to. Null only on the local row, whose node is whatever this server runs as. |
| `store_id` | text | 32 lowercase hex digits. Unique with `node_id`. |
| `local` | integer | 1 for this database's own row, else 0. |
| `state` | text | `active`, `retired` or `conflicted`. |
| `first_seen_at`, `last_push_at` | integer | When the row was made, and the last push received from that store. |
| `next_seq` | integer | The next sequence number this store will stamp; set on the local row only, and at least 1. Each outage open and end takes one. |

A row with `local = 0` is made the first time a batch from that store is
applied. Applying a batch is idempotent: results and outages already stored
under their origin key are skipped, an outage change that matches what is
stored is a no-op, and one that differs is refused as a conflict with
nothing written. Outage changes must arrive in seq order, after any already
applied from that store. A result dated before `purged_before` is skipped and
counted; one dated past `FUTURE_TOLERANCE` stops that kind of row for the
batch, keeping the ones before it. Rollups another node pushed are stored as
they came, and the local recompute counts only this node's raw results and
those of nodes with no `node_store` row, so it never overwrites them.

### `push_cursor`

How far each upstream this store pushes to has acknowledged its rows, added
by 0019. One row per upstream.

| Column | Type | Notes |
|---|---|---|
| `upstream_id` | text | Primary key; non-empty. |
| `results_through` | integer | The last `check_result.id` the upstream committed; 0 for none. |
| `outages_through` | integer | The last outage seq the upstream committed; 0 for none. |
| `rollups_from` | text | `YYYY-MM-DD`: the first day whose rollups it may not have yet, or null. |
| `acked_at` | integer | When it last acknowledged, or null. |

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
