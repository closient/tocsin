# Alert hub

Status: **proposal** (TOC-91, 2026-10-26). Nothing here is built. This
page settles how tocsin would take in alerts from Prometheus and other
senders, show them on one page, group them, silence them and route them,
so that Alertmanager can be demoted; and it lists the tickets that would
build it. [DESIGN.md](../../DESIGN.md) stays the decision record; it
points here.

## The problem

Christian, 2026-10-25:

> The part of all this I am most excited about is having a single place I
> can look to see what is alerting and can silence them. The whole thing
> about getting message after message about testing-03 power supply is
> driving me nuts. AlertManager has an absolutely blechourous design and
> karma does not make it much better.

tocsin already alerts on its own checks and heartbeats, holds those alerts
with silences (TOC-90) and can post them to an Alertmanager (TOC-87). What
it cannot do is the reverse: take in the alerts Prometheus rules and other
senders raise, which at Closient are most of them. So there are two places
to look, two silence systems, and Alertmanager's routing decides who is
woken.

The testing-03 case shows what goes wrong. The rule
`HardwareChassisSensorDegraded` (Closient
`deploy/prometheus/rules/hardware.yml:47-72`) fires once per IPMI sensor
series, and the series carry `sensor`, `sensor_id`, `sensor_type` and
`state` as labels (Closient
`ansible/files/closient_hardware_telemetry.py:1407-1418`). One failed power
supply is two series ("Power Supply 2: Disabled", sensor 36h, and "Power
Supplies: Redundancy Lost", sensor 38h), so two alerts; a change in the
`state` text mints a third, since the label set is the identity. The rule
is `page_class="page"`, `needs_human="true"`, `daily_repeat="true"`, which
in Closient's `deploy/alertmanager/alertmanager.yml` means:

- the Home Assistant on-call path, grouped per alert (`group_by: ['...']`)
  and repeated every hour (`alertmanager.yml:106-112`);
- Pushover, repeated every 24 hours (`:141-144`);
- email, every 24 hours (`:153-156`);
- Slack, every 24 hours (`:167-169`).

That is one fault arriving as several alerts on four channels, each with
its own repeat clock, and silencing it means writing an Alertmanager
silence whose matchers cover every series, through Karma or `amtool`.

The ticket asks for:

1. **Ingest**: an Alertmanager-compatible `POST /api/v2/alerts`; fires and
   resolves pair by label identity.
2. **One view**: a page of every active alert with its labels, source,
   start time and silenced state.
3. **Grouping**: by labels such as host, alertname and sensor type, so one
   fault is one item and one notification.
4. **Silences** over ingested alerts, under the TOC-90 rules, set in one
   action from the view.
5. **Routing** to Pushover, the Home Assistant on-call path, Slack and
   email, with `daily_repeat` honoured, so Alertmanager is demoted or
   removed.

Acceptance, from the ticket: testing-03's `HardwareChassisSensorDegraded`
appears as one item and one tocsin silence stops every channel; a
Prometheus resolve clears the alert, with identical labels; and a cutover
plan loses no page class, with the alert sweep's census moving to tocsin.

## What exists today

Every claim below is about the code on `main` at the time of writing
(313e16f, release 0.6.0).

- **Alerts are about tocsin's own subjects only.** An alert row names a
  `subject_kind` that is `check` or `heartbeat`, a `subject_id`, a
  notifier and an `outage_id NOT NULL REFERENCES outage`
  (`crates/tocsin-store/migrations/0005_alerting.sql:42-63`). At most one
  is open per `(node_id, subject_kind, subject_id, notifier_id)`
  (`0005_alerting.sql:66`). The outage itself is keyed on a check or
  heartbeat (`0005_alerting.sql:5-10`). Nothing in the schema can hold an
  alert that came from outside.
- **Alerts are opened and ended inside the outage transaction.**
  `Store::change_outage` (`crates/tocsin-store/src/lib.rs:411`, body in
  `crates/tocsin-store/src/alert.rs:304`) writes the outage change, the
  alert rows (`fire_alerts`, `alert.rs:490`; `end_alerts`, `alert.rs:595`)
  and their outbox messages in one transaction, so a crash cannot lose a
  resolve that was decided. Each alert's messages are queued in
  `alert_delivery` as `fire`, `repeat`, `resolve` or `cancel`, with status
  `pending`, `delivered`, `failed` or `silenced`
  (`crates/tocsin-store/migrations/0014_silences.sql:86-90`).
- **Labels are pinned per alert, as JSON.** `alert.labels` and
  `alert.annotations` are JSON objects fixed when the alert opens
  (`crates/tocsin-store/migrations/0015_alert_labels.sql`). They are
  computed from config by the TOC-87 label contract: the identity labels
  `alertname`, `node`, `subject_kind`, `subject_id`
  (`crates/tocsin-core/src/config/alerting.rs:13`) plus `[alerting]
  labels.<name>`, with names checked against Prometheus's rules
  (`alerting.rs:73`).
- **Routes match subjects, not labels.** A `[[route]]` names notifiers and
  matches by `components`, `groups`, `checks`, `heartbeats` and
  `notify_on`, with a `repeat` (default 4h, `"0s"` for never)
  (`crates/tocsin-core/src/config/mod.rs:554-579`;
  `crates/tocsin-alert/src/route.rs:16-25`). Every matching route applies;
  when several send one alert to the same notifier it is sent once, on the
  shortest repeat (`route.rs:203-246`; [config.md](../config.md)). There
  is no grouping, no inhibition and no "stop here" route.
- **Silences match five ids by equality.** `Matchers` holds `node`,
  `subject_kind`, `subject`, `component` and `notifier`, each optional
  (`crates/tocsin-core/src/silence.rs:27-44`); `matches()` compares with
  `==` only (`silence.rs:74-91`). A silence may be scoped to one episode
  (`episode REFERENCES outage`), must carry a reason and a mandatory
  expiry within `[server] max_silence` (default 24h,
  `crates/tocsin-core/src/config/mod.rs:189-229`), records who made it,
  and can only be ended, never edited, enforced by the `silence_only_ends`
  trigger (`0014_silences.sql`). The worker checks silences as each
  message comes due (`crates/tocsin-alert/src/worker.rs:301`), holds a
  matching one (`worker.rs:509`) and fires a held alert once its silence
  lapses (`worker.rs:472`). Silences are made through
  `POST /api/v1/silences` with the `silence` token scope
  (`crates/tocsin/src/http/silences.rs`), the CLI and MCP; there is no
  form on any page. The operator's account is
  [alerting.md](../alerting.md#silences).
- **Outbound to Alertmanager, not inbound.** The `alertmanager` notifier
  posts `POST /api/v2/alerts` every `REPOST_INTERVAL` (60s) with `endsAt`
  `FIRING_HOLD` (5m) ahead, and on resolve sets `endsAt` to the end time
  (`crates/tocsin-alert/src/notifiers/alertmanager.rs:36-38`). tocsin
  serves no `/api/v2/` route.
- **Notifiers.** Slack posts through an incoming webhook, configured or
  connected on the admin page (`notifiers/slack.rs:1-2`). Pushover maps
  each health to a priority; emergency priority repeats every 60s for an
  hour, both fixed (`notifiers/pushover.rs:22-26`), and a resolve cancels
  the emergency by tag (`pushover.rs:5-7`). Email threads fire, repeats and
  resolve with one `References`, and the mailer refuses a `noreply`-style
  From or Reply-To (`crates/tocsin-alert/src/mail.rs:62-76`). The generic
  webhook sends tocsin's own JSON signed with HMAC-SHA256
  (`notifiers/webhook.rs:1-6`); nothing sends Alertmanager's webhook
  shape.
- **Tokens.** `TokenScope` is `heartbeat`, `incident`, `incident:draft`,
  `release`, `status:read`, `subscribers:admin` and `silence`
  (`crates/tocsin-core/src/config/mod.rs:604`); a route asks for one with
  `Authorized<scope::X>` (`crates/tocsin/src/http/auth.rs`), answering 401
  for an unknown token and 403 for a missing scope.
- **Pages.** `/admin/` writes incidents and `/internal/` is the read-only
  board of every check and heartbeat, both behind the proxy sign-in
  ([admin.md](../admin.md)). The board already carries two writes, a
  draft's **Publish** and **Discard**, which post to admin routes with the
  admin sign-in and CSRF token ([admin.md](../admin.md#the-internal-board)).
- **Durability.** Everything except a no-change probe result is committed
  before it is acknowledged (DESIGN.md, Durability).

And at Closient, which this design must carry over without losing a page:

- **Prometheus** evaluates every 30s (`deploy/prometheus/prometheus.yml:7-8`)
  and sends to one Alertmanager (`prometheus.yml:38-41`), with the default
  resend delay of 1m. 86 rules across 15 files in
  `deploy/prometheus/rules/`.
- **Other senders** push straight to Alertmanager: `post_alert.py` (ops
  jobs, GitHub Actions, Django, backup units), the Bugsink health checks
  and UptimeFlare. The label contract (`scripts/alerting/README.md`)
  requires `alertname`, `severity`, `page_class`, `remediable`,
  `needs_human`, `blast_radius`, `source` and `host_name`, with optional
  `daily_repeat`, `container_name` and `service`. `post_alert.py` sends a
  firing alert with `startsAt = now` and `endsAt = now + ttl`, refusing a
  missing ttl, and a resolve with `endsAt = now`
  (`scripts/alerting/post_alert.py:216-226`). Backup units post a resolve
  after every successful run whether or not anything fired
  (`deploy/backup/object-backup-alert.sh:383`).
- **Alertmanager** routes to seven receivers, not five: `history` (a
  webhook to the alert-history relay that writes VictoriaLogs),
  `homeassistant` (the site alarm), `homeassistant-oncall`, `email`,
  `pushover`, `pushover-emergency` and `slack` (`alertmanager.yml:176-272`).
  The full table is under [Routing parity](#routing-parity).
- **tocsin is already in parallel run.** Closient's tocsin check failures
  reach Alertmanager through Prometheus rules over tocsin's `/metrics`
  (`tocsin_check_up`), every one labelled `parallel_run="true"`, which a
  route without `continue` sends to Slack only (`alertmanager.yml:73-74`;
  `deploy/prometheus/rules/tocsin.yml`). The decommission flip for when
  Gatus goes deletes that label and that route (`tocsin.yml:34-39`), so by
  the time this cutover starts those rules page through Alertmanager like
  any other. Closient's tocsin has one native route of its own: the
  Alertmanager watchdog check to Pushover
  (`deploy/tocsin/config/notifiers.toml:26-30`).
- **The alert sweep** reads Alertmanager:
  `GET /api/v2/alerts?active=true&silenced=true&inhibited=true` and
  `GET /api/v2/silences`, using `fingerprint`, `labels`, `annotations`,
  `startsAt`, `endsAt`, `status.state`, `status.silencedBy`,
  `status.inhibitedBy` and the silences' `updatedAt`
  (`.claude/commands/alert-sweep.md`; `.claude/scripts/alert_sweep_*.py`).
  "Resolved" is a fingerprint that was in the last tick and is not in this
  one. kthxbye extends silences whose comment starts `ACK!`.

## Rules any design must keep

- **Every alert has a matching resolve with the same identity**
  (AGENTS.md). For an ingested alert that includes one whose sender went
  quiet without resolving it.
- **Files are the only config; the database holds history.** Routes,
  groupings and inhibit rules are config. An ingested alert and a silence
  are data.
- **Nothing acknowledged is lost.** An ingest post is committed before its
  2xx, as every other write is.
- **The TOC-90 silence rules**: an expiry is mandatory and capped by
  `max_silence`; the reason and who made it are recorded; a silence is
  never edited or extended, only ended; no `ACK!` prefix semantics.
- **Every email can be answered**: the email notifier keeps its
  `noreply` refusal (`mail.rs:71-74`).
- **JSON is never the source of truth for anything queried.** Ingested
  labels are matched by silences, groupings, routes and inhibit rules, so
  they get a table, not a JSON column.
- **Something outside tocsin still pages when tocsin is down.** A hub
  that is the only path cannot report its own death (DESIGN.md,
  Milestones).

## Two shapes

### A. tocsin is the hub

tocsin takes in every alert, groups, shows, silences and routes them all
through its own notifiers. Alertmanager is kept only as the path for
"tocsin itself is down" (see [Watching the hub](#watching-the-hub)).

### B. tocsin is the view; Alertmanager still routes

tocsin takes in every alert, groups, shows and silences them, and mirrors
each silence into Alertmanager over its v2 silences API so Alertmanager's
routing stays the only thing that notifies.

| | A. Hub | B. View only |
|---|---|---|
| One place to look | yes | yes |
| One silence stops every channel | yes | only if every mirrored silence lands; two silence stores to reconcile |
| One notification per fault | yes, tocsin groups | no, Alertmanager's grouping and repeats stay |
| TOC-90 rules hold | yes | no: an Alertmanager silence can be made, edited or extended (kthxbye) outside tocsin |
| Work | ingest, view, silences, grouping, routing parity | ingest, view, silences, a silence mirror |
| Alertmanager | demoted to a dead-man path | stays the router |

B fails two of the three acceptance criteria on its own: the routing that
sends "message after message" stays where it is, and a silence that lives
in two systems is a silence that can disagree with itself. B is useful
only as a stage: tocsin ingesting and showing everything while
Alertmanager still delivers is how the cutover can compare the two.

**Recommendation: A, reached through B as the middle phases of the
cutover.**

## Ingest

### The route

`POST /api/v2/alerts`, taking Alertmanager's v2 body: a JSON array of
`{labels, annotations, startsAt?, endsAt?, generatorURL?}`. Prometheus
needs no change beyond the target and a credential:

```yaml
alerting:
  alertmanagers:
    - api_version: v2
      authorization:
        type: Bearer
        credentials_file: /etc/prometheus/tocsin-ingest-token
      static_configs:
        - targets: ['tocsin:8080']
```

It needs a new token scope, `alerts:ingest`, so a sender's token can post
alerts and do nothing else; an unknown token is 401 and a token without the
scope 403, as today. Ingest is off unless `[ingest]` is present, and the
route is then a 404 like any unknown path.

### What Prometheus sends, and when

From Prometheus's rule evaluator (`rules/alerting.go` at v3.15.0, the
version Closient runs):

- A firing alert is sent when it has not been sent for the resend delay
  (`needsSending`, lines 102-113), default 1m. Pending alerts are never sent.
- Each send sets `ValidUntil`, which becomes `endsAt`, to
  `now + 4 × max(resendDelay, evaluation interval)`, "to allow for two
  eval or Alertmanager send failures" (lines 619-625). At Closient that is
  `now + 4m`.
- `startsAt` is the time the alert became active and does not change while
  it fires.
- Once resolved, the alert is sent with `endsAt` = the resolve time, and
  re-sent on every resend for `resolvedRetention`, 15 minutes (lines
  381-383, 510).

`post_alert.py` differs in one way that matters: every firing post carries
`startsAt = now`.

### The rules

Identity is the full label set, as in Alertmanager. tocsin stores the
fingerprint as Alertmanager computes it, a 64-bit FNV-1a hash over the
sorted label names and values (`LabelSet.Fingerprint` in
prometheus/common), so the sweep's fingerprints survive the switch. The
ingest ticket verifies this against prometheus/common's test vectors
before relying on it.

For each alert in a post, in one transaction:

1. **Validate.** `alertname` present; every name a Prometheus label name
   (`is_label_name`, `alerting.rs:73`); no more than 64 labels; a value no
   longer than 1 KiB; annotations no more than 16 KiB in all. A missing
   `startsAt` is the receive time. A missing `endsAt` on a firing alert is
   `now + [ingest] resolve_timeout` (default 5m, Alertmanager's
   `resolve_timeout`, which Closient sets to 5m at `alertmanager.yml:31`).
2. **Firing** (`endsAt` in the future): if an episode with this
   fingerprint is open, move its `ends_at` to the new `endsAt` and keep the
   earlier `startsAt` (as Alertmanager's merge does), so `post_alert.py`'s
   moving `startsAt` does not restart the clock. If none is open, open one,
   unless `startsAt` is not after the end of the last episode with this
   fingerprint: that is a late copy of an episode already over, and is
   dropped.
3. **Resolved** (`endsAt` at or before the receive time): if an episode is
   open and `endsAt` is not before its `starts_at`, end it at `endsAt` with
   ending `resolved`. A resolve whose `endsAt` is before the open episode's
   start is one of Prometheus's 15-minute re-sends of the previous episode,
   and must not close the new one: it is dropped. A resolve with nothing
   open is a no-op; it is common (every successful backup run sends one)
   and is counted, not logged.
4. **Commit, then answer.** The whole post is one transaction. Valid alerts
   are stored even when others in the post are not; the answer is then
   400 naming each rejected alert and why, so a bad sender is loud without
   dropping the good alerts beside it. A repeated post changes nothing but
   `ends_at` and `last_seen_at`, so retries are safe.

### Expiry: the resolve nobody sent

An episode whose `ends_at` passes with no new post is ended by tocsin with
ending `expired`, and its group resolves if it was the last member (see
[Every alert has a resolve](#every-alert-has-a-resolve)). This is what
Alertmanager does silently; tocsin does it audibly: the resolve message
says the sender stopped reporting rather than that the fault cleared, and
`tocsin_ingest_expired_total{alertname}` counts it. At Closient,
`AlertSourceSilent` already watches the senders.

### Restart

Open episodes are rows, so a restart forgets nothing. One trap: an
episode whose `ends_at` lapsed while tocsin was down would expire on boot
and then reopen a minute later, when Prometheus next resends, as a new
episode with a fresh fire. So the expiry sweep waits `[ingest] boot_grace`
(default 2m, two Prometheus resends) after start before it ends anything;
posts during the grace extend episodes as usual. Prometheus does not
retry a failed send, but it resends every firing alert each minute, so
the first resend after tocsin is back extends or reopens what was missed;
a restart shorter than the 4-minute hold loses nothing.

### Where it lives in the schema

Two new tables, not a widening of `outage`, because an ingested alert has
no check, no component and no `degraded`/`down` health:

- `ingested_alert`: `id`, `fingerprint`, `group_id`, `starts_at`,
  `ends_at` (the expiry deadline), `last_seen_at`, `ended_at`, `ending`
  (`resolved`, `expired`, `withdrawn`), `generator_url`, `token_id` (the
  sender), `annotations` (JSON, display only). Unique on `fingerprint`
  while `ended_at IS NULL`. Rows only end, like outages.
- `ingested_label`: `(alert_id, name, value)`, primary key
  `(alert_id, name)`, indexed on `(name, value)`. Labels are fixed for the
  row's life (a changed label set is a different fingerprint).

Grouping adds `alert_group` and `alert_group_label` (below). The existing
`alert` table gains `subject_kind = 'group'` and a nullable `group_id`,
with `outage_id` nullable and a CHECK that exactly one of them is set; this
is a table rebuild, as `0014_silences.sql` did for `alert_delivery`. That
lets an ingested group use the worker, outbox, retries, repeats, silences
and refire that check alerts already have, unchanged. A group's `health`
is `down` when any open member matches `[ingest] down_when` (default
`severity="critical"`) and `degraded` otherwise, so the existing
per-health notifier settings, Pushover priorities among them, apply.

Ended episodes follow the retention of outages ([storage.md](../storage.md)).

## One view

`/internal/alerts`, beside the board and behind the same sign-in, with a
count of what is firing in the board's header. It lists **items**: an
ingested group, or an open alert on one of tocsin's own checks or
heartbeats, so both kinds are in one place. Each item shows:

- its group labels, how many members are open, and since when;
- its state: firing, silenced (by which silence, until when, made by whom
  and why) or inhibited (by which item);
- which notifiers it reached and when, from `alert_delivery`;
- the members, folded: each one's full labels, annotations
  (`summary`, `runbook_url`, `human_action` as links and text), sender,
  `startsAt` and expiry deadline;
- a **Silence** form: a duration (1h, 4h, 1d, or up to `max_silence`), a
  required reason, and whether to cover only this episode or every future
  alert with these group labels. It posts to `/admin/silences` with the
  admin sign-in and CSRF token, as a draft's **Publish** does.

Under the items, the active silences, each with an **Expire** button, and
the last day's ended items.

DESIGN.md's UI decision says the board "reads, never writes" and that a
check is changed only in its file. The silence form fits it as the draft
buttons do: it writes data through an admin route, not config, and the
board's own pages stay read-only.

The same data is served as JSON twice: `GET /api/v1/alerts` in tocsin's
own shape, and `GET /api/v2/alerts` and `GET /api/v2/silences` in
Alertmanager's (`fingerprint`, `status.state` as `active` or
`suppressed`, `status.silencedBy`, `status.inhibitedBy`, `receivers` as
the notifiers a fire was delivered to), both with `status:read`. The
second pair is what lets the sweep move by changing a URL.

## Grouping

A group is the unit of the view, of notification and of the one-action
silence. Config:

```toml
[ingest]
group_by = ["..."]                     # the default: every alert its own group
group_wait = "30s"                     # the default wait

[[ingest.group]]
matchers = ['alertname="HardwareChassisSensorDegraded"']
by = ["alertname", "host_name", "sensor_type"]

[[ingest.group]]
matchers = ['alertname="UptimeFlareMonitorDown"']
group_wait = "0s"                      # still one group per alert
```

The first `[[ingest.group]]` whose matchers match an alert decides its
`by` and `group_wait`; otherwise the `[ingest]` defaults. `["..."]` means
the full label set, as in Alertmanager, and is the default: every alert
is its own group, and only an explicit `[[ingest.group]]` aggregates.

**Grouping must never absorb a new fault silently.** A member that joins
an open group does not notify (below), and a group silence holds members
that join later (see [Silences](#silences)). So a group's matchers and
`by` labels must be narrow enough that any alert joining it is the same
fault, as testing-03's power-supply group is: two sensors reporting one
failed supply. A broad grouping such as `["alertname", "host_name"]`
would let a second disk or fan failing on a host that already has an open
group of that alertname arrive with no message and under the first
fault's silence; tocsin therefore does not default to one, and the docs
for `[[ingest.group]]` state this rule beside the key. The group's identity is the `by` labels with
their values (a label the alert lacks counts as empty). Those labels are
the group's labels, and they are what its fire and resolve carry.

How a group behaves, and where it differs from Alertmanager:

- **First fire waits `group_wait`** so alerts raised together land in one
  message. It belongs to the group, not the route, because how long to
  gather is a property of the fault class; a class that must ring at once
  sets `0s`.
- **Members joining or leaving do not re-notify.** Alertmanager re-sends a
  group whenever its membership changes (after `group_interval`); that is
  a large part of "message after message". In tocsin a group fires once,
  repeats on its route's `repeat`, and resolves once. A route can opt back
  in with `on_member_change = true`; the history receiver needs it (see
  [Routing parity](#routing-parity)).
- **A group ends when its last member ends.** Its resolve carries the group
  labels and says whether the last member resolved or expired. A member
  arriving afterwards opens a new episode of the group with a new fire.

### testing-03, worked

The two series of testing-03's failed power supply, as Prometheus posts
them (other labels omitted):

```
alertname=HardwareChassisSensorDegraded host_name=testing-03 sensor_id=36h
  sensor="Power Supply 2" sensor_type="Power Supply" state="Disabled"
alertname=HardwareChassisSensorDegraded host_name=testing-03 sensor_id=38h
  sensor="Power Supplies" sensor_type="Power Supply" state="Redundancy Lost"
```

Two fingerprints, two `ingested_alert` rows. With the group above, both
have group labels `alertname=HardwareChassisSensorDegraded
host_name=testing-03 sensor_type="Power Supply"`: one `alert_group`, one
item in the view with two members, one fire per notifier, one daily repeat
from the `daily_repeat` route. If the BMC changes the `state` text, the old
member ends and a new one joins the same open group: no message.

The one-action silence on that item writes label matchers equal to the
group labels. It holds the group now and any member that joins later,
which an episode silence cannot do, and it stops every channel because
every channel's message goes through the same `alert` rows the worker
holds. The part may take weeks; Closient sets `max_silence` to 30 days
(decided, see [Open questions](#open-questions)), so one silence can
cover the wait, and it still expires.

Without that `[[ingest.group]]`, the default keeps the two series as two
items, each notifying and each silenced on its own; that is the safe
failure. A fan failing on testing-03 has `sensor_type="Fan"`, so it opens
its own group and notifies even while the power-supply group is open and
silenced.

Whether the redundancy sensor's `sensor_type` is "Power Supply" depends on
the BMC; the telemetry script keeps a disabled sensor only when its type
is "Power Supply" (`closient_hardware_telemetry.py:1402`), but does not say
what type the redundancy sensor reports. The grouping ticket checks the
live series. If the two differ, the group stays as narrow by its
matchers instead (`sensor=~"Power Suppl.*"` added, `by = ["alertname",
"host_name"]`), never by widening `by` to the whole alertname on the
host.

## Silences

Silences keep every TOC-90 rule and gain one kind of matcher.

- **Label matchers.** A silence may carry any number of
  `{name, op, value}` with `op` one of `=`, `!=`, `=~`, `!~`; regexes are
  anchored, as Alertmanager's are, and compiled with the `regex` crate,
  which like Alertmanager's RE2 has no backreferences. An absent label
  matches as the empty string, which is how Closient's `page_class=""`
  means "no page_class" (`alertmanager.yml:99`). Stored in
  `silence_label_matcher (silence_id, name, op, value)`, insert-only under
  the same trigger discipline as `silence`.
- **The one-action group silence holds future members by design.** It
  writes matchers equal to the group labels, so it covers an alert that
  joins the group after it was made; that is what stops testing-03's
  `state` changes from ringing. It is also why only deliberately narrow
  groups exist (see [Grouping](#grouping)): the silence is exactly as
  wide as the group. On a default, per-alert group it equals the alert's
  full label set and holds that alert alone.
- **What a label silence holds.** An ingested group is held when every
  open member matches; a member that does not match keeps the group
  notifying, and the view marks each member silenced or not. Label
  matchers also apply to tocsin's own alerts, against the labels pinned
  when the alert opened (`0015_alert_labels.sql`).
- **At least one matcher that cannot match everything.** The existing
  "at least one matcher" CHECK (`0014_silences.sql:34`) cannot see the new
  table, so the store enforces in the same transaction that a silence has
  an id matcher or a positive label matcher whose value does not match
  the empty string, as Alertmanager does.
- **Episodes.** "This episode only" sets `episode_group` to the group's
  current episode; the silence then ends when the group does, as an
  outage episode silence does today (`ACTIVE`,
  `crates/tocsin-store/src/silence.rs:30`).
- **Unchanged**: reason required; expiry required and capped by
  `max_silence`; `created_by` recorded; no edit, no extension (a longer
  silence is a new one, and the old one is expired); no `ACK!` prefix.
- **The cap.** `max_silence` stays one config key with tocsin's default
  of 24h. Closient sets it to 30 days (Christian, 2026-10-27), long enough
  for a hardware fault waiting on a part, and safe only because of the
  grouping rule: a silence is exactly as wide as a deliberately narrow
  group.

## Routing parity

Routes gain label matchers, written as Alertmanager writes them so
Closient's can be copied:

```toml
[[route]]
id = "page-daily"
matchers = ['page_class="page"', 'needs_human!="false"', 'daily_repeat="true"']
notifiers = ["pushover"]
repeat = "24h"
```

A route with `matchers` applies to ingested groups (matched against every
open member: a route applies when any member matches) and, through their
pinned labels, to tocsin's own alerts. Every matching route still applies,
with one message per notifier on the shortest repeat (`route.rs:203-246`).
That rule is why tocsin needs no "stop" key for Closient: Alertmanager's
two routes without `continue` either go away (`parallel_run`, below) or
are made exclusive with a negative matcher.

Closient's tree, route by route (`alertmanager.yml:44-170`; root
`group_by [alertname, host_name, container_name]`, `group_wait 30s`,
`group_interval 5m`, `repeat_interval 6h`). The tocsin column keeps each
route's Alertmanager matchers unless it says otherwise, including
`alertname!="HomeAssistantPagingPathDown"` on every page route, so that
alert rings only through its own route (#6) and once:

| # | Alertmanager | tocsin | Gap |
|---|---|---|---|
| 1 | every alert → `history`, per alert, `continue` (`:61-66`) | route, no matchers → `history` webhook, `on_member_change = true`, `repeat = "0s"` | Alertmanager-shaped webhook; `on_member_change` |
| 2 | `parallel_run="true"` → `slack`, **stops** (`:73-74`) | none: the Gatus decommission flip deletes the route and the label before this cutover (`tocsin.yml:34-39`) | none |
| 3 | `UptimeFlareMonitorDown` on production → `homeassistant`, per alert, `continue` (`:82-88`) | route with those matchers → `homeassistant` webhook, `repeat = "0s"`; its `[[ingest.group]]` is per alert with `group_wait = "0s"` | Alertmanager-shaped webhook |
| 4 | `page_class=""`, not `HomeAssistantPagingPathDown` → `homeassistant-oncall`, 1h (`:99-105`) | route → `homeassistant-oncall`, `repeat = "1h"` | label matchers; Alertmanager-shaped webhook |
| 5 | `page_class="page"`, `needs_human!="false"`, not HAPPD → `homeassistant-oncall`, 1h (`:106-112`) | same | same |
| 6 | `HomeAssistantPagingPathDown` → `pushover-emergency`, 1h (`:122-126`) | route → a second Pushover notifier at emergency priority, `repeat = "1h"` | per-notifier emergency `retry`/`expire` (Alertmanager 5m/15m, tocsin fixed 60s/1h) |
| 7 | `page_class=""` → `pushover` (6h) (`:130-132`) | route → `pushover`, `repeat = "6h"` | label matchers |
| 8 | page, needs_human, `daily_repeat="true"` → `pushover`, 24h (`:141-144`) | route → `pushover`, `repeat = "24h"` | none beyond matchers |
| 9 | page, needs_human, `daily_repeat!="true"` → `pushover` (6h) (`:145-147`) | route → `pushover`, `repeat = "6h"` | none beyond matchers |
| 10 | `severity=~"critical\|warning"` → `email`, 24h (`:153-156`) | route → `email`, `repeat = "24h"` | regex matchers; annotations in the body |
| 11 | `page_class=~"page\|"`, `severity!~"critical\|warning"` → `email`, 24h (`:161-164`) | same | same |
| 12 | `daily_repeat="true"` → `slack`, 24h, **stops** (`:167-169`) | route → `slack`, `repeat = "24h"` | none beyond matchers |
| 13 | catch-all → `slack` (6h) (`:170`) | route `daily_repeat!="true"` → `slack`, `repeat = "6h"` | the negative matcher replaces #12's stop |

Rows 12 and 13 are the one place the shortest-repeat rule would bite: if
the catch-all also matched daily-repeat alerts, Slack would repeat them
every 6h. The `daily_repeat!="true"` matcher on the catch-all keeps
`daily_repeat` honoured.

Receivers:

| Receiver | Alertmanager | tocsin today | Gap |
|---|---|---|---|
| `history` | webhook to the alert-history relay, which splits Alertmanager's grouped body into one VictoriaLogs record per alert (`deploy/alert-history/relay.py`) | webhook sends tocsin JSON, HMAC-signed | an `alertmanager_webhook` notifier sending Alertmanager's body (`status`, `groupLabels`, `alerts[]`) |
| `homeassistant`, `homeassistant-oncall` | Home Assistant cloudhooks, Alertmanager's body, read from `url_file` | as above | the same notifier, with the URL from `${ENV:…}` or a file |
| `email` | Google SMTP from `team@`, Reply-To `support@`, to `christian@`, Closient templates | email with mandatory Reply-To (`mail.rs:62-76`) | the body shows the contract annotations (`human_action`, `runbook_url`, `summary`) |
| `pushover` | priority 1, Closient templates | priority per health | none for priority; the annotations in the message |
| `pushover-emergency` | priority 2, retry 5m, expire 15m; cannot cancel | emergency fixed at 60s/1h (`pushover.rs:22-26`); cancels by tag on resolve | configurable `retry` and `expire` |
| `slack` | `chat.postMessage` with a bot token to `#alerts`, title linking to Karma | incoming webhook or connected workspace | the link goes to the item on `/internal/alerts`; whether the bot-token path is needed is settled in the Slack ticket |

Inhibition, which tocsin lacks entirely: Closient has two rules
(`alertmanager.yml:35-40`), `HostTelemetrySilent` and
`TelemetryIngestSilent` each inhibiting `OpsJobNotRunning` (the first only
on the same `host_name`). tocsin adds `[[inhibit]]` with
`source_matchers`, `target_matchers` and `equal`. An inhibited group
notifies nobody and shows as inhibited, with `alert_delivery` status
`inhibited` naming the source group, so it is visible and auditable like a
silence.

## Every alert has a resolve

For ingested alerts the rule is about the group, since the group is what
notifies:

- A group that fired resolves exactly once, when its last member ends,
  through every notifier its fire reached, with the same group labels.
  The worker's resolve path is the existing one; the group's `alert` rows
  end in the same transaction as the last member.
- A member that ends by **expiry** ends the group like a resolve, with the
  message saying it expired. A member that ends because its sender
  **resolved** it says so.
- A silenced group still resolves, as a silenced check alert does
  today: a held fire that was never delivered gets no resolve
  (`alert.notified`, `0014_silences.sql`), one that was delivered does.
- If config changes so that a route no longer matches, the alerts it
  opened end with a resolve, as `end_alerts_without_notifier` does for a
  removed notifier (`lib.rs:488`).
- `[ingest]` removed or the sender's token revoked: open episodes are
  ended `withdrawn` and their groups resolve. A revoked sender cannot
  leave an alert open forever.
- The `alertmanager` notifier, forwarding to another Alertmanager, sends
  the group labels on both fire and resolve; tested, as TOC-87's are.

## Watching the hub

When tocsin is the hub, tocsin down means nothing pages, and the meta
alerts about tocsin (`TocsinExporterDown`, `TocsinNotificationsAbandoned`
and the rest of `tocsin.yml`) would be posted to the thing they report on.
So Alertmanager is **demoted, not removed**: Prometheus keeps it as a
second target (Prometheus sends every alert to every listed Alertmanager),
and its config shrinks to one route, the tocsin meta alerts
(`TocsinExporterDown`, `TocsinChecksStale`, `TocsinCheckNeverRan`,
`TocsinConfigReloadFailed`, `TocsinNotificationsAbandoned`) to Pushover at
emergency priority, and a null receiver for everything else, including
`HomeAssistantPagingPathDown`, which tocsin owns (#6 above) so it can be
seen and silenced with the rest. tocsin's native watchdog route keeps
checking the demoted Alertmanager, so each watches the other. With UptimeFlare outside and the
sweep's own direct page when its census source is unreachable, that is
three independent paths for "the hub is down".

**One failure, counted once.** Closient's tocsin check failures keep
arriving as the `tocsin.yml` Prometheus rules, ingested like any other
alert; Closient's tocsin config gains no native routes for its own checks
beyond the watchdog, so a failing check is one item, not two. Retiring
those rules in favour of tocsin's native alerts with TOC-87 labels is a
later, separate choice. Federation's nodes
watching each other ([federation.md](federation.md#nodes-watching-each-other))
is what would let the demoted Alertmanager go.

## Cutover

After the Gatus-to-tocsin switchover (due 2026-11-03). Each phase has a
rollback and a check that no page class went missing. The classes are
those of the label contract: page (needs_human true), page
(needs_human false), digest, and absent `page_class` (pages, by
`alertmanager.yml:99` and `:130`).

0. **Build.** The tickets below, released. Nothing at Closient changes.
1. **Dual-send.** Prometheus lists tocsin as a second Alertmanager;
   `post_alert.py`, the Bugsink health checks, the backup units and
   UptimeFlare post to both. tocsin's ingest routes point only at a test
   Slack channel. *Rollback*: remove the second target. *Check*: none
   needed; Alertmanager is untouched.
2. **Compare for a week.** A census script diffs tocsin's
   `GET /api/v2/alerts` against Alertmanager's every minute by
   fingerprint, and logs every difference in open set, start time and
   silenced state. *Exit*: a week with no unexplained difference, and at
   least one resolve and one expiry of each page class seen in both.
3. **Silences move.** Closient's tocsin config sets `[server]
   max_silence = "30d"`. Silences are made in tocsin only, each for at
   most 30 days; Alertmanager's remaining silences are left to expire. kthxbye's `ACK!` extension is
   switched off, since it breaks the no-extension rule. *Rollback*: make
   silences in Alertmanager again. *Check*: no Alertmanager silence
   newer than the switch.
4. **Receivers move one at a time**, each by enabling the tocsin route
   and deleting the Alertmanager route in the same change: Slack, email,
   history, `homeassistant`, `homeassistant-oncall`, then Pushover and
   `pushover-emergency`. *Rollback*: restore the Alertmanager route and
   disable the tocsin one. *Check*: before and after each move, a test
   alert of every page class (`post_alert.py --name AlertHubCutoverProbe`
   with each class's labels) reaches the expected receivers, from the
   destination side: the phone, the HA log, the inbox, the channel.
5. **The sweep's census moves.** `alert_sweep_fetch.sh` points at tocsin's
   `/api/v2/alerts` and `/api/v2/silences`; `page_suppressed` reads
   `receivers` instead of inferring that Alertmanager rang. *Rollback*:
   point it back. *Check*: the fingerprints of the first tocsin tick
   equal the last Alertmanager tick's, so nothing reads as resolved and
   re-fired.
6. **Alertmanager demoted** to the dead-man config above. Karma goes. *Rollback*:
   restore the previous Alertmanager config from git. *Check*: stop
   tocsin on a maintenance window and see the dead-man page arrive.

## What changes in the schema and API

**Migrations** (the next free numbers): `ingested_alert`,
`ingested_label`, `alert_group`, `alert_group_label`; `alert` rebuilt with
`subject_kind` `'group'`, a nullable `group_id` and `outage_id`;
`silence_label_matcher` and `silence.episode_group`; `alert_delivery`
status `inhibited` with the inhibiting group.

**HTTP**: `POST /api/v2/alerts` (`alerts:ingest`); `GET /api/v2/alerts`,
`GET /api/v2/silences` and `GET /api/v1/alerts` (`status:read`);
`POST /api/v1/silences` accepts `labels`; `/internal/alerts`;
`POST /admin/silences` and `/admin/silences/{id}/expire`. Every new route
is classified in the router enumeration test as internal.

**Config**: `[ingest]` (`group_by`, `group_wait`, `resolve_timeout`,
`boot_grace`, `down_when`), `[[ingest.group]]` (`matchers`, `by`,
`group_wait`), `[[route]] matchers` and
`on_member_change`, `[[inhibit]]`, a notifier type `alertmanager_webhook`,
Pushover `emergency_retry` and `emergency_expire`.

**Metrics**: `tocsin_ingest_alerts_total{result}` (opened, extended,
resolved, dropped_stale, rejected), `tocsin_ingest_expired_total{alertname}`,
`tocsin_ingest_open` and `tocsin_alert_groups_open`; documented in
[metrics.md](../metrics.md), which its test checks.

**CLI and MCP**: `tocsin alerts list`, and `silence new --label`; MCP
`list_alerts`, and label matchers on `create_silence`.

## Out of scope

- Alertmanager's API beyond what Prometheus and the sweep use: no
  `/api/v2/status`, no receivers listing, no amtool compatibility.
- Editing routes or groupings from a page. They are config.
- Notification templates as a language. Messages show the contract
  annotations in a fixed layout.
- Clustering two tocsins for ingest. That is federation's.
- Ingesting from anything that cannot speak Alertmanager's v2 post.

## Recommendation

1. tocsin ingests Alertmanager v2 posts with Alertmanager's identity and
   fingerprint, stores them in label tables, expires what goes quiet with
   a resolve that says so, and serves an Alertmanager-shaped read API.
2. Every alert is its own group unless a deliberately narrow
   `[[ingest.group]]` aggregates it, so no new fault joins a group
   silently; a group is the one item in the view, the one notification
   and the one silence, and does not re-notify when members come and go.
3. Silences gain label matchers and keep every TOC-90 rule; the view sets
   one in one action.
4. Routes gain Alertmanager-style matchers; with negative matchers
   Closient's tree maps without a stop key; gaps are inhibition, an
   Alertmanager-shaped webhook and Pushover emergency timing.
5. Alertmanager is demoted to a dead-man path, after a six-phase cutover
   in which each receiver moves alone and is checked at its destination.

## Proposed follow-up tickets

Each ticket carries its own documentation and tests; there is no separate
docs ticket.

**1. Ingest Alertmanager v2 alerts.**
`POST /api/v2/alerts` under a new `alerts:ingest` scope, `[ingest]`
config, the `ingested_alert` and `ingested_label` tables, the rules under
[Ingest](#the-rules) (merge keeping the earlier `startsAt`, stale-resolve
drop, no-op orphan resolve, partial 400), expiry with `boot_grace`, the
fingerprint computed as prometheus/common does, and the metrics. No
notification yet: ingested alerts are stored and counted only.
Acceptance: a test replays a recorded Prometheus sequence (fire, resends
with rolling `endsAt`, resolve, 15 minutes of resolved re-sends, re-fire)
and ends with exactly two episodes; a fingerprint matches
prometheus/common's for its test vectors; a restart inside `boot_grace`
expires nothing.

**2. Alert groups and their notifications.**
`alert_group` and its labels, `[ingest] group_by` defaulting to
`["..."]` (every alert its own group), `[[ingest.group]]` with the
narrowness rule in its documentation, the `alert` table
rebuild for `subject_kind = 'group'`, `group_wait`, `down_when`, group
fire, repeat and resolve through the existing worker, `on_member_change`,
and route `matchers` with `=`, `!=`, `=~`, `!~` for groups and for
tocsin's own alerts' pinned labels.
Acceptance: the testing-03 pair of series is one group and one fire per
notifier under its `[[ingest.group]]`; a member's state change sends
nothing; the group resolves once, with the group labels, when the last
member resolves or expires; with no `[[ingest.group]]`, two alerts of the
same alertname on the same host are two items and both fire, and a fan
alert on testing-03 fires while the power-supply group is open and
silenced.

**3. Label matchers on silences.**
`silence_label_matcher`, the cannot-match-everything rule, the
every-member rule for groups, `episode_group`, and `labels` on the API,
CLI and MCP.
Acceptance: one silence on the testing-03 group labels holds every
channel's message, including for a member that joins after it was made;
the silence cannot be edited or extended; a silence of only `!=` matchers
is refused.

**4. The alerts view.**
`/internal/alerts`, the board header count, `GET /api/v1/alerts`, the
silence form and expire button on `/admin/silences`, with sign-in, CSRF
and the router classification test.
Acceptance: the page reads in full without JavaScript; a silence made from
it records the signed-in person and reason; a request without sign-in is
403.

**5. Alertmanager-shaped read API.**
`GET /api/v2/alerts` (with `active`, `silenced`, `inhibited` filters) and
`GET /api/v2/silences` in Alertmanager's shapes, `receivers` from
delivered fires.
Acceptance: recorded Alertmanager responses of the fields the sweep
reads, copied into tocsin's tests as fixtures, match tocsin's output field
for field; `status.state` and `silencedBy` agree with tocsin's own view.

**6. Inhibit rules.**
`[[inhibit]]` with `source_matchers`, `target_matchers`, `equal`; the
`inhibited` delivery status; inhibited items in the view and the v2 API.
Acceptance: Closient's two rules, written as tocsin config, inhibit
`OpsJobNotRunning` on the same host only while the source fires.

**7. Notifier parity for Alertmanager's receivers.**
An `alertmanager_webhook` notifier sending Alertmanager's webhook body;
Pushover `emergency_retry` and `emergency_expire`; the contract
annotations in Pushover, email and Slack messages; the Slack link to the
item.
Acceptance: the alert-history relay and a Home Assistant cloudhook accept
the body unchanged (recorded payloads in tests); an emergency at 5m/15m is
sent with those values.

**8. Closient cutover runbook and census diff.**
Docs only on the tocsin side: the phases above as a runbook, the
dual-send Prometheus config, the census diff script's contract, and the
dead-man Alertmanager config. The Closient changes themselves are
Closient tickets, one per phase.
Acceptance: each phase has its rollback and its destination-side check
written as commands.

## Open questions

1. **How long may a silence last?** *Decided (Christian, 2026-10-27)*:
   Closient's `max_silence` is 30 days, expiry stays mandatory, and the
   cap stays a single config key; tocsin's default stays 24h. The
   question was that `max_silence` is 24h by default and the TOC-90 rules
   forbid extending a silence, while testing-03's part may take weeks; a
   daily new silence is the toil this ticket is meant to end. The 30-day
   cap leans on the
   grouping rule: a long group silence is safe only because groups are
   per alert by default and aggregate only where every member is the same
   fault, so it cannot hide a new one for a month.
2. **Demote or remove Alertmanager?** *Recommended*: demote, as above,
   until two tocsin nodes watch each other; a hub with no outside path
   cannot report its own death.
3. **Regex matchers, or equality only?** *Recommended*: regex, anchored.
   Closient's routes use `=~` and `!~` (`alertmanager.yml:82`, `:153`,
   `:161`), and without them the email routes do not map.
4. **Sweep: compat API or an adapter?** *Recommended*: the
   Alertmanager-shaped `GET /api/v2/alerts`, so the census moves by a URL
   change and the sweep's tested parsers stay as they are.
5. **Where does alert history live afterwards?** Alertmanager keeps none,
   which is why the history relay exists; tocsin keeps every episode.
   *Recommended*: keep feeding the relay through the
   `alertmanager_webhook` notifier until the Alert History dashboard can
   read tocsin, then retire the relay in its own Closient ticket.
