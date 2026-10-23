# The engine

`tocsin serve` runs every configured check on a schedule, stores each
result, decides when a check's state has really changed, keeps outage
records, works out what the status page shows for each component, and
announces every change on the server's event bus. This is the engine,
in `crates/tocsin-engine`; the rules it applies are pure functions in
`crates/tocsin-core/src/state.rs`.

## Scheduling

Each check runs on its own task, every `interval`. Its first run is not at
start-up but after a fixed offset between zero and a tenth of the
interval, derived from the check's ID alone (FNV-1a, then a 64-bit
finaliser, at millisecond granularity). The same ID gets the same offset
on every node and in every release, so a restart does not reshuffle the
schedule, and checks sharing an interval spread across its first tenth
instead of all firing together.

A run that cannot start on time, because the previous one or the global
limit held it up, is skipped rather than queued: a slow check never builds
up a backlog of runs that then fire back to back.

No more than `server.max_concurrent_checks` probes (default 64) run at
once across all checks. A check waiting for a slot is not timed out; its
`timeout` starts when its probe does.

The probe enforces the check's `timeout` itself: one that has not
finished in time is down, with error kind `timeout`, and the result keeps
the probe's own start time and duration. The engine adds a backstop of
five seconds beyond the timeout; a probe still running then is a bug in
the probe, logged at `error`, and is recorded down with kind `timeout`.

A probe that cannot run on this machine at all is down with error kind
`setup`, every time, so the problem shows on the page instead of hiding.
That covers an ICMP check without permission to open the socket, and
every `http` and `tls` check when the machine has no root certificates.
In that case `serve` logs one error at start-up naming the affected
checks and keeps running everything else; installing the certificates
takes a restart. (`tocsin probe run` still exits 3 for such a check.)

## Results and confirmation

Every result is stored in `check_result`, whatever it means for the
check's state.

A check's state changes only after `confirm` consecutive results agree
that it is no longer what it was: each differs from the current state. The
new state is the health of the result that completes the run, so a check
that is up and then reports `degraded, down` with `confirm = 2` goes to
`down`. A result equal to the current state resets the run: with
`confirm = 3`, a check that is up stays up through
`down, down, up, down, down`, because no run of three ever completes.

A change carries:

- `from`: the state before, or `null` for a check's first state since
  the engine started. The first state also needs `confirm` consecutive
  results that agree with each other, and it is the health they agree on:
  with `confirm = 2`, `up, down` sets nothing, and a following `down`
  makes the check down.
- `since_ms`: when the first result of the confirming run started. The
  outage records use this time, so an outage starts when the trouble did,
  not when it was confirmed.
- `confirmed_at_ms`: when the result that completed the run started.

## Outages

Leaving `up` opens an outage record for the check; returning to `up` ends
it. A move between `degraded` and `down` ends one outage and opens another
at the same instant, because an outage's health never changes. At most one
outage per check and node is open at a time; the store enforces this.

Ending one outage and opening the next are one transaction. The engine
commits the new state only after that write succeeds; if it fails, the
error is logged, the check keeps its old state, and the next result that
confirms the change retries the write.

On start the engine seeds every configured check, emitting no events:

1. A check with an open outage for this node resumes in that outage's
   state, with the outage's start as `since_ms`. A restart in the middle
   of an outage therefore neither opens a second outage nor forgets the
   first; the next confirmed change ends it as usual.
2. Otherwise, a check with a recent up result resumes as up. With no open
   outage the check was not confirmed degraded or down, and a down result
   since then has not confirmed anything yet, so the latest up result is
   the evidence. Recent means within `confirm` intervals plus the start-up
   offset's tenth of an interval, the time the check would need to confirm
   a state afresh. `since_ms` is when the check last recovered, or its
   first stored result if it has never been down.
3. Otherwise the check has no state until it confirms one.

An open outage for a check no longer configured is left open, with a
warning in the log.

Migration `0003_outage_alerted` adds `outage.alerted`, which records
whether the change that opened the outage alerted, so a recovery after a
restart is paired correctly (see below). Open incidents are a read of the
`incident` table.

## Component status

A component's health comes from the checks that name it, combined by its
`rule`:

- `worst` (the default): the worst state among its checks.
- `majority`: the worst state that more than half of the checks with a
  state are in or beyond. Three checks at `up, up, down` are `up`;
  `up, down, down` are `down`; `up, degraded, down` are `degraded`. A tie
  falls to the better state: one of two checks down leaves the component
  up.

A check with no state yet does not count. A component with no checks, or
whose checks all lack a state, has no health yet and shows `unknown`
unless an incident or maintenance says otherwise. (Whether an empty
component should be allowed at all is for configuration validation.)

What the status page shows is then:

1. During an active maintenance window covering the component:
   `maintenance`, unless an open incident naming the component declares an
   impact above `none`, in which case that impact shows (the checks'
   health stays hidden either way).
2. Otherwise the worse of the checks' health and the worst open incident's
   impact. An incident never makes a component look better than its
   checks.
3. With neither health nor an incident: `unknown`.

An incident's impact maps onto health as `none` → up, `minor` →
degraded, `major` and `critical` → down. The mapping is lossy on purpose:
the component shows down for both; the incident itself still says which.

## Incidents and maintenance windows

The engine reads the unresolved incidents and uncompleted maintenance
windows from the store when it starts, whenever an incident is created,
updated or edited (it listens on the event bus), at each maintenance
window's start and end, and at least once a minute.

A maintenance window is its schedule: active from `scheduled_start` until
`scheduled_end`, whatever its status says, except that marking it
`completed` ends it at once.

During maintenance, check state changes and outages are recorded as
usual, and each check change carries `maintenance: true`.

Whether a change alerts is decided once, when it is recorded, and stored
on the outage it opens, so every alert gets its resolve and no resolve
arrives without its alert:

- A change to `degraded` or `down` alerts if the outage it ends alerted,
  or if no maintenance window covers the check's component. A move
  between `degraded` and `down` during maintenance therefore keeps an
  existing alert open instead of dropping it.
- A change to `up` alerts exactly when the outage it ends alerted, whether
  or not a window is active at that moment. An outage raised before a
  window resolves inside it; one silenced by a window stays silent when
  it ends after the window.
- A first state of `up` ends nothing and never alerts.

## Events

Each confirmed change is announced exactly once, on the same bus as the
incident events:

- `check_changed`: `check_id`, `component_id`, `from`, `to`, `since_ms`,
  `confirmed_at_ms`, `ended_outage_id`, `opened_outage_id`,
  `maintenance` (whether a window was active) and `alert` (the decision
  above).
- `component_changed`: `component_id`, `from`, `to` (`operational`,
  `degraded`, `down`, `maintenance` or `unknown`), `health` (from the
  checks alone, which `to` may hide or override) and `at_ms`.

A result that changes nothing emits nothing.

## Errors

A failed write of a result or an outage is logged at `error` and the
engine carries on probing: a monitor that stops monitoring because its
disk is full is worse than one that says so. A failed read of incidents
keeps the last ones read, also logged at `error`. Reading the open
outages, recent results or incidents at start-up is required: if it fails, `serve` exits
with code 3.

On SIGTERM or SIGINT, `serve` stops the engine before draining HTTP and
closing the database, so no probe writes into a closed store. A probe in
flight is abandoned, not recorded.

## Extending it

- **Alerting**: subscribe to the event bus and act on `check_changed`
  where `CheckChange::alerts()` is true. `ended_outage_id` and
  `opened_outage_id` pair a recovery with the outage it ends.
- **Status page**: `Engine::snapshot()` gives every check's state and last
  result and every component's status; `component_changed` on the bus
  says when to redraw.
- **Metrics**: the same snapshot for gauges, and the bus for counters of
  changes.
- **Reloading configuration**: build a new `Plan` from the new
  configuration, `shutdown()` the running engine and `Engine::start` the
  new one with the same parts. Seeding carries each check's state
  across, so a reload loses no state that matters; the confirmation run
  in progress is the only thing that starts over. Seeding is
  `seed_checks` in `engine.rs`.
