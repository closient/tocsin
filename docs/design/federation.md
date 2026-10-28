# Federation

Status: **proposal** (TOC-55, 2026-10-23). Nothing here is built. This
page compares three ways for tocsin nodes to share history, picks one, and
lists the tickets that would build it. [DESIGN.md](../../DESIGN.md) stays
the decision record; it points here.

## The problem

tocsin runs as one node today, and that stays the default. Christian named
two later shapes (2026-10-22):

1. **Nodes behind a firewall or in a DMZ report to a central location.**
   The DMZ node can open connections out but takes none in.
2. **Nodes share their data with each other**, so each has the whole
   picture.

Two more wants fall out of having more than one node:

3. **Multi-location checks**: the same check run from several nodes, with
   "down" decided by a quorum instead of by one vantage point.
4. **Nodes watch each other**, so a deployment can drop the outside
   watchdog. Today UptimeFlare watches the tocsin node, because "a single
   node cannot report its own death" (DESIGN.md, Milestones).

## What exists today

Every claim below is about the code on `main` at the time of writing.

- **Node identity.** `[server] node_id` names the node in every row it
  writes. It defaults to the host name (`crates/tocsin/src/config.rs:84`)
  and is checked only for being non-empty with no whitespace
  (`crates/tocsin-core/src/config/load.rs:728`). `[server]` is read once at
  startup; a reload keeps the running one (`crates/tocsin/src/reload.rs`,
  module comment). Config ids, by contrast, must match
  `^[a-z0-9][a-z0-9-]*$` (`crates/tocsin-core/src/config/values.rs:22`).
- **Results are append-only and carry the node.** `check_result` has
  `node_id NOT NULL`, a trigger that aborts every `UPDATE`, and an index
  `check_result_by_node (node_id, started_at)` whose comment already says
  "federation" (`crates/tocsin-store/migrations/0001_history.sql`).
- **Outages change once.** The only change to an `outage` row is setting
  `ended_at` and `ending` together, once (`outage_only_ends`, rewritten by
  `0003`, `0005` and `0008`). `outage_open` allows one open outage per
  `(subject_kind, check_id, node_id)` (`0005_alerting.sql:10`).
- **Row ids are local.** `check_result.id` is `AUTOINCREMENT`;
  `outage.id` is a plain `INTEGER PRIMARY KEY`; `alert.outage_id` points at
  the local outage id. No column holds an id that means anything on
  another node.
- **Commit order is id order.** One writer thread commits every write
  ([storage.md](../storage.md#connections)), so a reader that has seen
  `max(id) = n` never later sees a new row below `n`. Retention already
  relies on this.
- **Rollups are per node and summed on the page.** `daily_rollup` is keyed
  by `(check_id, node_id, date)`. The 90-day bar sums every node's
  rollups for a check (`crates/tocsin/src/web/uptime.rs:10`).
- **Finalized days refuse results.** `check_result_not_before_retention`
  aborts an insert dated before `purged_before` (`0001_history.sql`).
- **The clock-skew hook exists.** A result starting more than
  `FUTURE_TOLERANCE` (one hour) after the storing node's clock is refused
  with `FutureResult` (`crates/tocsin-store/src/lib.rs:76`).
- **The engine only looks at its own node.** It seeds from
  `open_outages(node_id)` and withdraws orphans from the same list
  (`crates/tocsin-engine/src/engine.rs:753`,
  `crates/tocsin-store/src/outage.rs:61`). Live state on the page and in
  `/api/v1/checks` is this node's `Engine::snapshot()`
  ([api.md](../api.md)).
- **Alert identity includes the node.** `alert_open` is unique on
  `(node_id, subject_kind, subject_id, notifier_id)`
  (`0005_alerting.sql:66`).
- **Auth is shared-secret bearer tokens.** `[[token]]` secrets with
  scopes, compared in constant time (`crates/tocsin/src/http/auth.rs`).
  Nothing identifies a node.
- **Durability.** Only a no-change probe result is acknowledged before it
  commits; every other write, including `Store::append`, is committed
  before its caller hears back (DESIGN.md, "Durability").
- **Some data is not append-only.** An incident is edited in place by
  `PATCH`; a release is one `INSERT … ON CONFLICT DO UPDATE`; subscribers
  confirm and are deleted; a Slack connection is replaced on reconnect
  ([storage.md](../storage.md#tables)).
- **`ring` is already in the tree** (0.17, through rustls), so Ed25519
  needs no new dependency.

## Rules any design must keep

- **Single node is unchanged.** With no `[[node]]` table in the config,
  tocsin behaves exactly as it does today, and the new migrations change
  no answer it gives.
- **Outbound only for spokes.** A node behind a firewall opens every
  connection itself.
- **tocsin never writes configuration** (AGENTS.md). A hub cannot push
  config to its spokes. A shared folder means git, Ansible or rsync,
  outside tocsin.
- **No Closient-run service in the path.** The reasoning that ruled out a
  shared Slack relay (DESIGN.md, TOC-68) applies: every hub belongs to
  the operator who runs it.
- **Break loudly.** Every failure mode below ends in a metric and an alert
  with a matching resolve, never in a silent gap or a stale "up".

## Three designs

### A. Hub and spoke, push

Each spoke runs its checks, stores its history in its own SQLite, and
posts its new rows to one hub over HTTPS. The hub stores them beside its
own and serves the status page, the API and the alerts for everyone.

- No inbound hole on any spoke.
- Every row has one writer, its origin node, and travels one hop. The hub
  never edits a spoke's row and never forwards it.
- The hub is a single point of failure for the page and for alerting. A
  spoke keeps monitoring and buffering while the hub is down, but nobody
  is told anything unless a spoke can alert on its own.

### B. Mesh gossip

Every node holds every node's rows. Nodes exchange what they have with
peers, and rows travel through intermediate nodes until all converge.

- Survives the loss of any node with full history on the rest.
- Outbound-only nodes must pull as well as push, and every node must
  relay rows it did not write. A relayed row needs a signature from its
  origin stored with it, or any node can forge any other's history.
- Every node commits every node's results through its one writer. The
  write path is already the most-tuned part of tocsin (storage.md: TOC-57,
  -59, -60, -62); multiplying its load by the node count is a real cost.
- Every node serves a page. Which URL is canonical, and which node pages a
  person, needs leader election or produces duplicate alerts from every
  node.
- Incidents, releases, subscribers and Slack connections are edited in
  place. Merging them between writers means last-writer-wins on wall
  clocks or redesigning them as event logs. That is the fact that rules
  full mesh out for now: the append-only groundwork covers results and
  outages, not the rest.

### C. Hybrid: every node pushes its own rows to a list of upstreams

The one primitive is A's push, generalised: a node pushes **its own rows
only** to every node it lists as an upstream. No node relays another's
rows.

- One hub: every spoke lists the hub. This is design A.
- Two hubs: every spoke lists both, and each hub lists the other. Both
  hold everything; spokes hold only their own. Either hub can serve the
  page and alert.
- A small mesh of reachable nodes: each lists all the others. With three
  to five nodes, full fan-out costs less than gossip and needs no relay.

| | A. Hub and spoke | B. Gossip | C. Push to upstreams |
|---|---|---|---|
| Spoke needs inbound | no | yes, or pull | no |
| Writers per row | one | one, but relayed | one |
| Who must trust whom | hub trusts spoke keys | everyone trusts every origin key on every row | each upstream trusts its pushers' keys |
| Writes per result, cluster-wide | 2 | N | 1 + upstreams |
| Hub loss | page and alerts gone | nothing lost | covered with two hubs |
| Incidents, releases | on the hub | need multi-writer merge | on the hub(s), see out of scope |
| New moving parts | push, ingest | push, pull, relay, signatures per row, anti-entropy | push, ingest |

## Identity and auth between nodes

**Options considered.**

- **Bearer tokens** (`[[token]]` with a new scope). Already built. But the
  secret is symmetric: the hub holds a value that can impersonate the
  spoke, and in a mesh every node can impersonate every other.
- **Mutual TLS.** Standard, and it hides the endpoint from anyone without
  a certificate. But Closient fronts tocsin with a Cloudflare Tunnel
  ([cloudflare-access.md](../cloudflare-access.md)), and most deployments
  sit behind some proxy; TLS ends there, so tocsin never sees the client
  certificate. It also needs a CA, issuance and rotation.
- **Signed requests, Ed25519.** One key pair per node. The private key
  comes from the environment like every secret; the public keys sit in
  the config folder, where they are safe to commit. Signatures survive
  any proxy. A hub's config holds nothing that can forge a spoke's data.

**Proposal: Ed25519-signed pushes.** An operator who wants mTLS or a
Cloudflare Access service token in front of the hub can add it at the
proxy; tocsin does not depend on it.

```toml
# The shared folder, on every node.
[federation]
default_nodes = ["central"]      # where a check with no `nodes` runs

[[node]]
id = "central"
url = "https://status.example.com"   # takes pushes; omit for outbound-only
public_keys = ["ed25519:5Y0r…"]      # two during a rotation
push_to = []

[[node]]
id = "dmz-1"
public_keys = ["ed25519:Qm9v…"]
push_to = ["central"]

[server]
node_key = "${ENV:TOCSIN_NODE_KEY}"  # same variable name everywhere, different value per host
```

- Each push carries `Tocsin-Node`, `Tocsin-Store` (below) and
  `Tocsin-Signature`, an Ed25519 signature over
  `tocsin-push-v1\n<node>\n<store>\n<sha256 of the body>`. The receiver
  checks it against that node's `public_keys`.
- **A node may push only rows whose `node_id` is its own.** The receiver
  refuses a batch holding any other node's row. This, plus no relay,
  is what keeps trust one hop deep.
- At startup a node derives its public key from `node_key` and refuses to
  start if that key is not listed under its own `[[node]]`. A key copied
  to the wrong host fails there, not at the hub.
- **Replay** is harmless by construction: applying a batch twice changes
  nothing (next section). So there is no timestamp window to tune, and a
  node with a bad clock is not locked out by its own signature. TLS
  provides confidentiality; results include internal checks' targets and
  errors.
- Rotation: add the new public key, deploy, switch the env var, remove
  the old key. Both verify during the overlap.

## Merging results and outages without conflicts

**Every row has exactly one writer: the node that created it.** Results
are never updated. An outage is created once and ended once, by its
origin. Nobody else edits either, and nothing is relayed. So merging is
set union plus one monotone step (open to ended), and no merge rule ever
compares clocks.

**Keys.** A row needs a key that means the same thing on every node.

- The natural key `(node_id, check_id, started_at)` is not unique. A
  heartbeat ping that lands exactly on its deadline records the miss
  stamped with the deadline and then the ping stamped with its arrival,
  the same millisecond ([engine.md](../engine.md#heartbeats)).
- The origin's local id is unique only for the life of its database. A
  node whose `tocsin.db` is wiped starts its ids again at 1, and the
  hub would drop its new rows as duplicates of old ones.

So each database gets a **store id**: 128 random bits made by a migration
on first open. The portable key is `(store id, origin id)`. A rebuilt
node keeps its node id and key but brings a new store id, so its rows
never collide with the old ones.

**Results.** The receiver inserts each with its key and ignores one it
already has. Insert-or-ignore is commutative and idempotent. Receiver
ids are new local ids, so storage.md's "id above n means committed later"
still holds on the receiver.

**Outages.** Opening is insert-or-ignore by key. Ending sets `ended_at`
and `ending` where the row is still open. An end that arrives for a row
already ended with the same values is a no-op. **An end with different
values is a conflict**: one writer cannot disagree with itself, so it
means two machines are writing as one node (below). The receiver refuses
the batch, marks the node `conflicted`, and alerts.

**Order.** `outage_open` allows one open outage per check and node. If
the end of outage 1 arrived after the open of outage 2, the insert would
fail. The origin therefore numbers its outage changes: a per-store
counter assigns `opened_seq` on insert and `ended_seq` on end, and the
pusher sends changes in that order. The origin never had two open at
once, so replaying its changes in its order never does either.

**Rollups belong to their origin too.** The receiver does not compute
rollups for another node; the origin pushes its own after each hourly
recompute, and the receiver upserts them by `(check_id, node_id, date)`.
This matters for long partitions: once the receiver has finalized a day,
its trigger refuses raw results for it, but the origin's rollup for that
day still arrives and is complete. The receiver's hourly recompute
skips every node that pushes to it (a `node_store` row other than its
own), so it never overwrites their rollups. The page's bar, which already sums nodes,
needs no change.

**Raw rows the receiver refuses.** A result dated before the receiver's
`purged_before` is skipped, counted, and reported in the response; the
cursor moves past it and its rollup covers it. A result from more than
`FUTURE_TOLERANCE` ahead stops results at that row (see clock skew);
an outage change that far ahead does the same for outages. A rollup for
a day that has not begun by then waits alone, since each day's row
stands by itself. Each kind has its own cursor, so the others still
apply, and the report names the earliest such time across kinds. A row
whose values the schema would refuse (an empty check, a negative
duration, an outage that is up, a rollup whose counts don't add up, an
end before its start) refuses the batch with the field named.

## Failure modes

### Clock skew

- Merging never compares clocks, so skew cannot corrupt a merge.
- Each push carries the origin's `sent_at`; the receiver records its own
  `received_at`. The difference is the node's offset plus transit, shown
  as `tocsin_node_clock_offset_seconds{node}`, a warning past 5 seconds
  and an alert (with resolve) past 60, both configurable.
- An origin more than an hour ahead hits `FutureResult`. The batch stops
  at that row for its kind and the node shows `skewed`. Nothing is skipped: the row is
  real, and it is accepted once the clocks agree.
- Freshness, liveness and quorum (below) are measured on the receiver's
  clock only, from `received_at`.
- The timeline orders remote events by origin time, so skew under the
  alert threshold can misorder events by up to that much.

### A hub that is down, and partitions

- A spoke keeps probing and storing. Its own SQLite is the buffer; the
  only new state is a cursor per upstream (the last result id and outage
  seq the upstream acknowledged).
- Pushes retry with backoff from one second to one minute, with jitter.
- On recovery the spoke sends its backlog in order, in batches of at most
  1000 rows. Each batch is one write on the hub's writer, so the hub's own
  probes still commit between batches.
- **The hub answers 2xx only once a batch is committed.** The spoke moves
  its cursor on that answer; an early acknowledgement could lose another
  node's rows for good. This is the "everything else" case of DESIGN.md's
  durability rule.
- A spoke's own retention may delete rows it has not pushed. Those are
  covered by its rollups, and counted in
  `tocsin_push_rows_expired_total{upstream}`.
- **Late outages.** An outage that arrives already ended is recorded and
  shown on the timeline but does not page anyone after the fact. One that
  arrives still open alerts, and the message says when the origin
  detected it and that it arrived late.
- **No stale "up".** A check run only on a node the hub has not heard from
  within its staleness window shows `unknown`, not its last state. The
  window is the node's liveness heartbeat (`push_interval` plus `grace`),
  the same signal that decides who pages under `alert_order`.

### A node that is decommissioned

- Mark it `retired = true` in its `[[node]]` (keep the entry, so the id
  is never reused for a different machine whose history would merge into
  the old one).
- The receiver refuses its pushes with 403, withdraws its open outages
  (`ending = withdrawn`, alerts resolve as ended by the configuration, as
  for a check removed from config: [alerting.md](../alerting.md)), stops its liveness
  watch, and keeps its history. The outages are withdrawn when the
  hub starts or reloads with the node retired.
- The bar sums nodes per day, so a retired node drops out from the first
  day it has no rollup; past days keep its share.

### A node that goes dark without being retired

Its liveness heartbeat (below) goes down and alerts. Its checks go
`unknown` after the staleness window. Its open outages stay open: nobody
saw them end.

### Duplicate node ids

This is the default failure, not an edge case, because `node_id` defaults
to the host name.

- **Two configs, one id, different keys**: impossible. A key verifies only
  for the `[[node]]` it is listed under.
- **One id and one key on two machines** (a cloned VM, a copied env
  file): two stores push as one node. The receiver tracks one active store
  per node. A new store id is accepted once, with a warning, as a rebuilt
  node: the old store's open outages are withdrawn when the new store's
  first batch lands. If the old store pushes again, the node is marked
  `conflicted`, both are refused, and it alerts until an operator retires
  one machine.
- **A host name that changes**: Docker gives a container its id as host
  name unless `--hostname` is set, so every recreated container is a new
  node. When federation is configured, `node_id` must be given
  explicitly, must match the id pattern, and must name a `[[node]]`.
  Startup refuses otherwise. The single-node case already suffers a
  milder form of this: a new node id after a recreate leaves the old id's
  open outages open forever, since seeding and withdrawal only read the
  current id (follow-up 1).

## Configuration: per node or a shared folder

**One shared folder, deployed to every node by whatever the operator
already uses** (git, Ansible, rsync). tocsin never ships config.

- Two things differ per node, and neither lives in a file: the node id
  (`tocsin serve --node <id>` or `TOCSIN_NODE_ID`) and the key
  (`${ENV:TOCSIN_NODE_KEY}`).
- A check says where it runs: `nodes = ["central", "dmz-1"]`. Without
  `nodes` it runs on `[federation] default_nodes`, which is required once
  any `[[node]]` exists. Each node starts only the checks naming it.
- A heartbeat names exactly one node, the one its job pings. Validation
  refuses more: two nodes watching one deadline would record two misses.
- **The receiving node's config decides presentation**: components,
  public or internal, the page. **The running node's config decides what
  runs.** A result for a check the hub's config does not know is stored
  but not shown, the rule that already applies to a removed check.
- **Drift is visible, not fatal.** Each push carries the digest of the
  sender's merged config (as `config show` prints it, secrets redacted).
  The hub shows each node's digest and exports
  `tocsin_node_config_match{node}`. A rolling deploy passes through
  drift, so it warns rather than refuses.
- Separate folders per node still work (a DMZ team owning its own), as
  long as check ids agree with the hub's.

## Multi-location checks with a quorum

```toml
[[check]]
id = "api"
nodes = ["central", "fra-1", "dmz-1"]
quorum = 2          # default: a majority of `nodes`
```

- Each listed node runs the check and confirms state with its own
  `confirm`, writing per-node results and outages exactly as today.
  Every node uses the same start offset for a check id
  ([engine.md](../engine.md#scheduling)), so the locations probe within
  milliseconds of each other.
- Wherever the rows converge (each hub), a pure function in
  `tocsin-core::state` derives the check's state from each node's
  confirmed state: **the worst health that at least `quorum` fresh nodes
  are at or beyond.** This is a sibling of the component `majority` rule
  (`crates/tocsin-core/src/state.rs:189`), with one difference that
  matters: `majority` counts only members with a state, so its bar drops
  as members go silent. A quorum is an absolute count, so a silent node
  never makes "down" easier to reach.
- **Fresh** means the hub holds a result for the check from that node
  that started within two of its intervals plus `[federation]
  push_interval` and `grace`, by the hub's clock: one late run, and the
  time a pushed result takes to arrive, do not lose it. A stale node is
  left out.
- A node evaluates a quorum check where every one of its nodes is that
  node or pushes to it. On every node the check runs on, its own runs are
  **evidence**: results and outages are written as always, but they never
  alert and never move the check's state or component. A node that runs
  the check without evaluating it never pages for it. The evaluator
  derives the state one window after start, then once per interval, and
  records each evaluation as a `*` result.
- Where more than one node evaluates a check, `[federation] alert_order`
  says which alerts: a node opens a `*` alert only when every node ahead
  of it is down, and a node ahead whose `node:<id>` heartbeat it does not
  keep counts as not up. Config refuses a check derived on two nodes that
  would each alert for it. A `*` outage opened while held back stays
  unalerted for its life.
- **Fewer than `quorum` fresh nodes**: the check is `unknown`, and the
  `quorum:<check>` heartbeat the evaluator keeps goes down, which alerts
  and resolves like any heartbeat. It carries the check's labels, class
  and links, routes as the check does, and has no component, so it is on
  the internal board only. The page never says "down" or "up" on too
  little evidence.
- **Derived outages** are stored as outages with `node_id = '*'` ("all
  locations"). That gives them their own alert identity under
  `alert_open` without colliding with any node's own outage, needs no
  table rebuild (a new `subject_kind` would mean rebuilding `outage` and
  `alert` to change their `CHECK`s), and `*` can never be a real node
  once node ids must match the id pattern. They are computed locally on
  each hub, never pushed.
- For a quorum check, the public surfaces show the `*` state and outages;
  per-node results and outages are visible with `status:read` and on the
  internal board. Alerts fire on `*` only, titled "on a quorum of nodes".
- The bar for a quorum check counts its `*` evaluations for each day that
  has any, not the sum of per-node results: one location's local trouble
  does not drag the bar while the quorum says up. A day from before the
  check had a quorum keeps its per-node rollups.

**Partitions, concretely.** A spoke that loses its route to the target
but not to the hub opens its own outage; with 2 of 3 the check stays up.
A spoke that loses the hub goes stale and is left out. A partition that
cuts the hub off from two of three locations yields "quorum lost", not
"api down".

## Nodes watching each other

- **A push is a heartbeat.** Each node pushes at least every
  `push_interval` (default 15 seconds), empty if it has nothing new. The
  receiver records, once the push commits and at most twice a
  `push_interval`, a ping of a synthetic heartbeat `node:<id>`. A colon
  cannot appear in a config id, so the name never collides. Its deadline,
  outage, alert and resolve are the existing heartbeat machinery
  ([heartbeats.md](../heartbeats.md#node-liveness)), with `interval` and
  `grace` from `[federation]`.
- **A pusher watches its upstream.** Each push that succeeds pings
  `upstream:<id>` on the pusher, the same way. With no push succeeding
  for `push_interval + grace` it opens, alerting through the pusher's own
  notifiers, and the next push that succeeds resolves it.
- **Who reports the hub's death.** Any node that pushes to it and can
  reach a notifier on its own path. A DMZ spoke usually cannot: the point
  of the hub was that the spoke has no way out but to it.
- **When to drop the outside watchdog.** Only when at least one other node
  (a) runs in a different failure domain from the hub (another host, power
  feed and network, ideally another provider), (b) can reach a notifier
  without the hub, and (c) is watched by the hub in return. The two-hub
  layout of design C meets all three. A single hub with DMZ spokes does
  not, and keeps UptimeFlare.

**Who pages.** With two hubs, both see every outage.
`[federation] alert_order = ["central", "backup"]` lists the nodes that
alert for remote outages. A node alerts for a remote outage only while
every node ahead of it in the list is down by its own liveness view; a
node it has never heard from, or not within `push_interval` plus `grace`,
counts as down.
Spokes not in the list that push to a node in it never alert for their
own checks and configured heartbeats (the hub does),
so nobody is paged twice. A partition between the two hubs makes both
alert: **a duplicate page beats a silent one.** A node keeps every alert
it fired until that alert resolves, even after an earlier node returns.
Liveness and upstream alerts are exempt: each watcher reports what it
sees.

The public page still dies with the hub that serves it. Putting two hubs
behind a failover is the operator's proxy configuration.

## What changes in the schema and API

**Migrations** (0018 on; all `STRICT`, enums as `CHECK`, no JSON
queried):

- `node_store`: `id`, `node_id`, `store_id` (32 hex characters), `local`
  (one row is this database's own, made on first open), `state`
  (`active`, `retired`, `conflicted`), `first_seen_at`, `last_push_at`,
  `next_seq` for the local row. Unique `(node_id, store_id)`.
- `check_result`: `origin_store` (references `node_store`) and
  `origin_id`, both null for local rows or both set; a partial unique
  index on `(origin_store, origin_id)` where set, so local inserts pay
  nothing for it.
- `outage`: the same two columns, plus `opened_seq` and `ended_seq`. The
  `outage_only_ends` trigger is recreated (as 0005 and 0008 did) to allow
  `ended_seq` alongside `ended_at`. Existing outages get seqs in id order.
- `push_cursor` on the pusher: `upstream_id`, `results_through`,
  `outages_through`, `rollups_from`, `acked_at`.
- `daily_rollup`: no change. Remote rows arrive by upsert; the local
  recompute skips nodes that push to it.
- Node liveness and quorum outages need no schema change (`node:<id>`
  heartbeats, `node_id = '*'`).

**HTTP**:

- `POST /api/v1/federation/batch`: signed, never anonymous. Body: node,
  store, `sent_at`, config digest, results, outage changes, rollups, and
  the sender's head positions. Answer: rows applied, rows skipped as
  expired, the new cursor. In the OpenAPI document; classified in the
  router enumeration test that TOC-69 added for anonymous routes. Not an
  MCP tool.
- `GET /api/v1/federation/nodes` (`status:read`): each node's state,
  store, last push by the receiver's clock, clock offset, backlog, config
  match and liveness. TOC-79 serves the state, stores, last push, clock
  offset and backlog. Liveness, read from the `node:<id>` and
  `upstream:<id>` heartbeats TOC-81 added, comes with the node board in
  TOC-80; config match is not served yet.

**Metrics** (labels from configuration only, per
[metrics.md](../metrics.md)): `tocsin_node_up`,
`tocsin_node_last_push_timestamp_seconds`,
`tocsin_node_clock_offset_seconds`, `tocsin_node_backlog_rows`,
`tocsin_node_config_match`, `tocsin_push_failures_total{upstream}`,
`tocsin_push_rows_expired_total{upstream}`.

**CLI**: `tocsin node keygen --json` (prints the public key for the
config and the private key once, for the secret store), `tocsin node
status --json` (HTTP client, as `tocsin status`). `tocsin config check`
validates `[[node]]`, `nodes`, `quorum` and `alert_order`.

## Out of scope

- **Replicating incidents, maintenance, releases, subscribers and Slack
  connections.** They change in place, so merging them needs a
  multi-writer rule. They stay on the hub that receives them. With two
  hubs, CI posts releases to both (each publish is an idempotent upsert),
  and incidents are written to the first hub in `alert_order`.
- Relaying another node's rows (gossip proper), and pull replication.
- tocsin distributing configuration.
- A hub or relay run by Closient for other people's nodes.
- Failing the public page over between hubs.
- Consensus or leader election. `alert_order` plus liveness is static
  ordering, accepted to duplicate under partition.
- Many tenants on one hub.

## Recommendation

Build **design C, push to upstreams**, starting with its single-hub case.
It meets the DMZ case with outbound connections only, keeps every row
single-writer and one hop from its origin so merging needs no clock and
no relay trust, and grows to two hubs that watch each other, which is
what lets a deployment drop its outside watchdog. Full gossip is rejected
for now because incidents, releases and subscribers are not append-only
and would need a multi-writer redesign, and because it multiplies the
load on the write path tocsin has spent most of its tuning on.

Order: node identity and keys, then the store and ingest, then liveness,
then quorum checks, then the second hub. Closient keeps UptimeFlare until
the second hub runs in a separate failure domain and has paged once in a
test.

## Proposed follow-up tickets

Each ticket carries its own documentation and tests; there is no separate
docs ticket.

**1. Explicit, validated node identity.**
When any `[[node]]` exists, `node_id` must be given (`--node`,
`TOCSIN_NODE_ID` or `[server] node_id`), match `^[a-z0-9][a-z0-9-]*$`,
and name a `[[node]]`; `*` and anything with a colon are reserved. Add the
store id (`node_store`'s local row, made on first open). For single-node
deployments, warn at startup when `node_id` came from the host name and
open outages exist under a different node id, naming them, since they will
never be seeded or withdrawn.
Acceptance: `config check` fails, with file, line and hint, on a federated
config whose `node_id` is defaulted, malformed or unlisted; a test opens a
fresh database twice and reads the same store id, and a wiped one gets a
different id; a test with an open outage under `old-host` and `node_id =
new-host` sees the warning naming it.

**2. Node keys and `[[node]]` configuration.**
`[[node]]` (`id`, `url`, `public_keys`, `push_to`, `retired`),
`[federation]` (`default_nodes`, `push_interval`, `grace`, `alert_order`),
`[server] node_key` as a secret; `tocsin node keygen`; signing and
verification with `ring`'s Ed25519; startup refuses a key not listed
under its own node. Schema and config reference regenerated.
Acceptance: a body signed by one node's key fails verification as
another node; a node whose `node_key` matches no listed key exits 2 at
startup naming the node; two public keys both verify during rotation;
`config show` redacts `node_key`.

**3. Store: portable keys and idempotent ingest.**
Migrations for `node_store`, origin columns, outage seqs and the
recreated trigger, and `push_cursor`. A store function that applies one
batch in one `IMMEDIATE` transaction: insert-or-ignore results and
outage opens, monotone ends, rollup upserts, expired rows skipped and
counted, a future row stopping its kind (a future rollup only itself), a
differing end refused as a
conflict. The local rollup recompute skips nodes that push to it.
Acceptance: applying a batch twice leaves the database byte-for-byte the
same as applying it once (row counts and contents), but for the
store's last push time; a batch whose outage
open precedes the previous outage's end in seq order applies, and one
reordered is refused; a differing end returns the conflict error and
writes nothing; a row before `purged_before` is skipped and counted; a
single-node database migrated to the new schema answers every existing
store test unchanged.

**4. Pushing: the origin side.**
A task per upstream that reads rows past its cursor in id and seq order,
batches up to 1000, signs, posts, moves the cursor on 2xx only, backs off
from one second to one minute with jitter, pushes rollups after each
hourly recompute, and sends an empty push every `push_interval`.
Acceptance: against a loopback hub that fails the first five posts, every
row arrives exactly once and in order; a hub that answers 2xx after
commit and then crashes loses nothing; a pusher killed mid-backlog
resumes from its committed cursor; retention deleting unpushed rows
increments `tocsin_push_rows_expired_total`.

**5. Ingest route and node status on the receiver.**
`POST /api/v1/federation/batch` (signature, own-rows-only, 2xx only after
commit, 403 for a retired node, 409 for a conflicted one) and `GET
/api/v1/federation/nodes`; the clock-offset and backlog metrics; the
OpenAPI document; the TOC-69 router classification.
Acceptance: a batch carrying another node's row is refused whole; a
retired node gets 403; the OpenAPI test and the router enumeration test
fail until the routes are registered and classified; the offset metric
reads within 1 s of an injected 30 s skew.

**6. Remote nodes on the hub's page, API and alerts.**
Remote checks' live state from received outages and results, `unknown`
past the staleness window; remote outages alert by `alert_order`, with
the late-outage rules (already ended: no page; open but late: page that
says so); the internal board lists nodes.
Acceptance: a remote check whose node stops pushing reads `unknown`, not
`up`, after the window; an outage received already ended creates no
alert delivery; one received open creates a fire and later its resolve
with the same identity; the second node in `alert_order` alerts only
while the first is down by its liveness.

**7. Node liveness and upstream watch.**
Synthetic heartbeats `node:<id>` on receivers and `upstream:<id>` on
pushers, through the existing heartbeat deadline, outage, alert and
resolve path.
Acceptance: stopping a spoke opens `node:<spoke>` after
`push_interval + grace` and alerts; restarting it resolves with the same
identity; blocking the hub opens `upstream:<hub>` on the spoke and its
notifier receives fire then resolve; neither id can be written in a
config file.

**8. Decommission, rebuilt stores and conflicts.**
`retired = true` refuses pushes, withdraws the node's open outages and
stops its liveness; a new store id for a known node is accepted once with
a warning and withdraws the old store's open outages; the old store
pushing again marks the node `conflicted` and alerts.
Acceptance: each of the three paths has a test that ends in the stated
outage, alert and node state; a conflicted node stays refused across a
hub restart until its config changes.

**9. Multi-location checks with a quorum.**
`nodes` and `quorum` on checks (heartbeats: exactly one node);
`state::quorum_health` beside `component_health`; derived `node_id = '*'`
outages and alerts; "quorum lost" with resolve; public surfaces show `*`
for quorum checks; the bar for a quorum check from its `*` evaluations.
Acceptance: with quorum 2 of 3, one node down leaves the check up and
pages nobody; two down opens one `*` outage and one alert; two stale
nodes give `unknown` and a "quorum lost" alert, not "down"; a property
test shows that adding a stale node never changes the derived health.

**10. A second hub, and retiring the outside watchdog at Closient.**
Run a second node in a separate failure domain, listing and listed by
the first; CI posts releases to both. Then a Closient change removes
UptimeFlare's watch of the tocsin node and updates DESIGN.md's
milestone.
Acceptance: stopping the first hub pages through the second within
`push_interval + grace`, and starting it again resolves; a staged
partition between the hubs pages from both and resolves on both; only
then does DESIGN.md drop UptimeFlare.
