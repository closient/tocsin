<!-- Generated from schema/tocsin.schema.json; do not edit. Regenerate with
     TOCSIN_UPDATE_DOCS=1 cargo test -p tocsin --test config_reference -->

# Configuration reference

Every key tocsin reads from its configuration folder, generated from [the JSON Schema](../schema/tocsin.schema.json) (`tocsin config schema`). [config.md](config.md) explains how the files fit together, with examples; this page is the list.

One configuration file. Every key is optional in a single file; rules that span files (unique ids, references, required settings) are checked after merging.

A **required** key must be set in its table. A key with no default and not required may be left out. `[[name]]` is an array of tables, written once per item; `[name]` is a single table.

## Tables

<a id="table-alerting"></a>

### `[alerting]`

`[alerting]`: the contract every check's and heartbeat's `labels` and `annotations` are held to. Without it, none is required and any value goes; the name and non-empty rules apply either way.

A receiver such as Alertmanager routes on labels, and a label that is missing or misspelt routes by default without anyone noticing. Declared here, the same mistake fails `tocsin config check`, and a reload that makes it keeps the last good configuration.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `annotations` | table of [table](#table-alertingrule) |  |  | The annotations the contract covers, by name, each a `[alerting.annotations.<name>]` table. Annotations it does not name are allowed and unchecked. |
| `labels` | table of [table](#table-alertingrule) |  |  | The labels the contract covers, by name, each a `[alerting.labels.<name>]` table. Labels it does not name are allowed and unchecked. |

<a id="table-alertingrule"></a>

#### `[alerting.annotations.<name>]`

Also `[alerting.labels.<name>]`.

What `[alerting]` asks of one label or annotation.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `required` | boolean |  | `false` | Whether every check and heartbeat must set it. |
| `values` | array of string |  |  | The values it may take, matched exactly. Empty allows any value. |

<a id="table-check"></a>

### `[[check]]`

A probe run on an interval, whose result sets a component's health.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `annotations` | table of string |  |  | Annotations on the check's alerts, such as a runbook: shown, not routed on. `[alerting]` can require some and limit their values. |
| `assert` | [array of tables](#table-assertion) |  |  | Conditions a result must meet, each `[[check.assert]]` table holding one. With none, a probe that completes counts as up, except that `http` needs a 2xx or 3xx status, `dns` a `NOERROR` response and `tls` a certificate that verifies for the host. |
| `class` | [`ProbeClass`](#type-probeclass) |  |  | What kind of probe this is: `production` (the default), `test`, `drill` or `canary`. Every alert of the check carries it as the `class` label, so a receiver can tell a drill from an outage without matching names. |
| `component` | [`Id`](#type-id) |  |  | The `[[component]]` whose health this check sets. |
| `confirm` | integer |  | `1` | Consecutive results that disagree with the current state before the state changes. |
| `dns` | [table](#table-dnssettings) |  |  | Settings for `dns` checks. |
| `http` | [table](#table-httpsettings) |  |  | Settings for `http` checks. |
| `id` | [`Id`](#type-id) | yes |  | Unique among checks and heartbeats in every file: they share one history. |
| `interval` | [`Duration`](#type-duration) |  | `"1m"` | How often the probe runs. |
| `labels` | table of string |  |  | Labels on the check's alerts, such as `{ team = "ops" }`: a receiver like Alertmanager routes on them. `[alerting]` can require some and limit their values. |
| `link` | [array of tables](#table-link) |  |  | Links shown with the check, such as its dashboard or runbook, each a `[[check.link]]` table. |
| `name` | string |  |  | Display name; the id is shown when it is absent. |
| `target` | string | yes |  | What is probed. Its form depends on the type: a URL for `http`, `host:port` for `tcp`, `host` or `host:port` for `tls`, a domain name for `dns` and a host for `icmp`. |
| `tcp` | [table](#table-tcpsettings) |  |  | Settings for `tcp` checks. |
| `timeout` | [`Duration`](#type-duration) |  | `"10s"` | How long one probe may take before it counts as failed. Must be shorter than the interval. |
| `tls` | [table](#table-tlssettings) |  |  | Settings for `tls` checks. |
| `topology_node` | [`TopologyNode`](#type-topologynode) |  |  | The node this check watches in a map of the estate, such as `operations-01`, so alerts can be grouped by what broke rather than by host names that drift. Every alert of the check carries it as the `topology_node` label; `[topology]` can limit the ids allowed. |
| `type` | [`CheckType`](#type-checktype) | yes |  | Which probe runs. |

<a id="table-assertion"></a>

#### `[[check.assert]]`

One condition a probe result must meet, and the state a failure maps to.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `banner` | [table](#table-textassertion) |  |  | The text a TCP server sends after connecting (and after `send`). |
| `body` | [table](#table-textassertion) |  |  | The response body text. |
| `certificate_days_remaining` | [table](#table-comparison) |  |  | Days until the server certificate expires. |
| `chain_valid` | boolean |  |  | Whether the server certificate chains to a trusted root and is within its validity period. |
| `header` | [table](#table-headerassertion) |  |  | A response header. |
| `hostname_valid` | boolean |  |  | Whether the server certificate is valid for the host name checked. |
| `json` | [table](#table-jsonassertion) |  |  | A value in the JSON response body. |
| `latency_ms` | [table](#table-comparison) |  |  | The probe's latency, in milliseconds. |
| `on_fail` | [`FailureState`](#type-failurestate) |  | `"down"` | The state the check takes when this condition fails. |
| `records` | [table](#table-recordsassertion) |  |  | The records a DNS lookup returns. |
| `status` | array of integer, ≤ 65535 |  |  | The HTTP status is one of these. |

<a id="table-textassertion"></a>

##### `[check.assert.banner]`

Also `[check.assert.body]`.

A condition on text.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `contains` | string |  |  | The text contains this. |
| `not_contains` | string |  |  | The text does not contain this. |
| `regex` | string |  |  | The text matches this regular expression (Rust `regex` syntax), anywhere in the text unless anchored with `^` or `$`. |

<a id="table-comparison"></a>

##### `[check.assert.certificate_days_remaining]`

Also `[check.assert.latency_ms]`.

Numeric bounds; every bound given must hold.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `eq` | integer |  |  | Equal to. |
| `gt` | integer |  |  | Greater than. |
| `gte` | integer |  |  | Greater than or equal to. |
| `lt` | integer |  |  | Less than. |
| `lte` | integer |  |  | Less than or equal to. |

<a id="table-headerassertion"></a>

##### `[check.assert.header]`

A condition on one response header.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `contains` | string |  |  | A value of the header contains this. |
| `eq` | string |  |  | A value of the header equals this. |
| `exists` | boolean |  |  | Whether the header is present. |
| `name` | string | yes |  | The header name, matched without regard to case. |

<a id="table-jsonassertion"></a>

##### `[check.assert.json]`

A value selected from the JSON response body by an RFC 9535 JSON path.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `contains` | string |  |  | The value is a string containing this. |
| `eq` | any |  |  | The value equals this. |
| `exists` | boolean |  |  | Whether the path selects any value. |
| `ne` | any |  |  | The value does not equal this. |
| `path` | string | yes |  | An RFC 9535 JSON path, such as `$.checks[0].status` or `$.components[?@.name=='API'].status`. When it selects several values, the first in document order is compared. |

<a id="table-recordsassertion"></a>

##### `[check.assert.records]`

A condition on the records a DNS lookup returns. Records are compared as text, ignoring a trailing `.` on names; CNAME, MX, NS and SRV records, which hold names, also ignore case.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `contains` | string |  |  | One of the records equals this. |
| `eq` | array of string |  |  | The records are exactly these, in any order. |

<a id="table-dnssettings"></a>

#### `[check.dns]`

`[check.dns]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `record_type` | [`RecordType`](#type-recordtype) |  | `"A"` | The record type queried. |
| `resolver` | string |  |  | The resolver queried, as `ip` or `ip:port`; the system resolver when absent. |

<a id="table-httpsettings"></a>

#### `[check.http]`

`[check.http]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `basic_auth` | [table](#table-basicauth) |  |  | Sent as HTTP basic authentication. |
| `bearer_token` | [`Secret`](#type-secret) |  |  | Sent as `Authorization: Bearer <token>`. Secret. |
| `body` | [`Text`](#type-text) |  |  | Request body. It may be a secret, `${ENV:NAME}`. |
| `follow_redirects` | boolean |  | `false` | Whether redirects are followed; when not, a 3xx is the result. |
| `headers` | table of [`Text`](#type-text) |  |  | Request headers. A value may be a secret, `${ENV:NAME}`. |
| `max_body` | [`ByteSize`](#type-bytesize) |  | `"64KiB"` | The most of the response body read, such as `"512KiB"`; the rest is not read, and assertions see only what was. At most 8MiB. Defaults to 64KiB. |
| `method` | [`HttpMethod`](#type-httpmethod) |  | `"GET"` | Request method. |
| `server_name` | string |  |  | The host name sent as TLS SNI and `Host`, and checked against the certificate, when it differs from the target's host. The target's host is still the address connected to. |
| `verify_certificate` | boolean |  | `true` | Whether the server's certificate must verify. Defaults to true. When false, the request proceeds and the certificate is still reported, so `chain_valid` and `hostname_valid` can be asserted. |

<a id="table-basicauth"></a>

##### `[check.http.basic_auth]`

`[check.http.basic_auth]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `password` | [`Secret`](#type-secret) | yes |  | The password. Secret. |
| `username` | [`Text`](#type-text) | yes |  | The user name. It may be a secret, `${ENV:NAME}`. |

<a id="table-link"></a>

#### `[[check.link]]`

Also `[[heartbeat.link]]`, `[[page.link]]`.

A `[[check.link]]` or `[[heartbeat.link]]`: a named link to somewhere an operator looks next, such as a dashboard, logs or a runbook. Also a `[[page.link]]`, shown to everyone in the status page's header.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `name` | string | yes |  | The link text. |
| `url` | string | yes |  | Where it goes: an absolute `http://` or `https://` URL. |

<a id="table-tcpsettings"></a>

#### `[check.tcp]`

`[check.tcp]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `send` | string |  |  | Text written once connected, before the banner is read. Escapes such as `\r\n` are written in TOML's own string syntax. |

<a id="table-tlssettings"></a>

#### `[check.tls]`

`[check.tls]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `server_name` | string |  |  | The server name sent in the handshake (SNI); the target's host when absent. |

<a id="table-component"></a>

### `[[component]]`

`[[component]]`: something the status page shows with a health state.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `description` | string |  |  | A line shown with the component. |
| `group` | [`Id`](#type-id) |  |  | The `[[group]]` the component is shown under. |
| `id` | [`Id`](#type-id) | yes |  | Unique among components in every file. |
| `name` | string | yes |  | Display name. |
| `public` | boolean |  | `true` | Whether the status page and the unauthenticated API show the component. A non-public component, its checks and heartbeats, their outages and its timeline are seen only with a `status:read` token and on the internal board, under its group like any other. No incident or maintenance may name one. |
| `rule` | [`ComponentRule`](#type-componentrule) |  | `"worst"` | How the health of the component's checks combines into its own. |

<a id="table-group"></a>

### `[[group]]`

`[[group]]`: a heading collecting components on the status page.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `description` | string |  |  | A line shown under the heading. |
| `id` | [`Id`](#type-id) | yes |  | Unique among groups in every file. |
| `name` | string | yes |  | Display name. |

<a id="table-heartbeat"></a>

### `[[heartbeat]]`

`[[heartbeat]]`: a ping tocsin expects to receive, such as a cron job reporting in. Pings are sent to the write API with a token that has the `heartbeat` scope. A heartbeat with no ping for `interval + grace` is down; it is a member of its component exactly as a check is.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `annotations` | table of string |  |  | Annotations on the heartbeat's alerts, such as a runbook: shown, not routed on. `[alerting]` can require some and limit their values. |
| `class` | [`ProbeClass`](#type-probeclass) |  |  | What kind of probe this is: `production` (the default), `test`, `drill` or `canary`. Every alert of the heartbeat carries it as the `class` label. |
| `component` | [`Id`](#type-id) |  |  | The `[[component]]` whose health this heartbeat sets. |
| `grace` | [`Duration`](#type-duration) |  | `"1m"` | How late a ping may be before the heartbeat is down: it is down once `interval + grace` passes without one. |
| `id` | [`Id`](#type-id) | yes |  | Unique among checks and heartbeats in every file: they share one history. |
| `interval` | [`Duration`](#type-duration) | yes |  | How often a ping is expected. |
| `labels` | table of string |  |  | Labels on the heartbeat's alerts, such as `{ team = "ops" }`: a receiver like Alertmanager routes on them. `[alerting]` can require some and limit their values. |
| `link` | [array of tables](#table-link) |  |  | Links shown with the heartbeat, such as the job's logs or runbook, each a `[[heartbeat.link]]` table. |
| `name` | string |  |  | Display name; the id is shown when it is absent. |
| `topology_node` | [`TopologyNode`](#type-topologynode) |  |  | The node this heartbeat watches in a map of the estate, such as `operations-01/job:backup`. Every alert of the heartbeat carries it as the `topology_node` label; `[topology]` can limit the ids allowed. |

<a id="table-notifier"></a>

### `[[notifier]]`

A destination for operator alerts. Every alert it sends is followed by a resolve with the same identity.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `alertmanager` | [table](#table-alertmanagersettings) |  |  | `[notifier.alertmanager]`. |
| `discord` | [table](#table-discordsettings) |  |  | `[notifier.discord]`. |
| `email` | [table](#table-emailsettings) |  |  | `[notifier.email]`. Sender addresses come from `[server.email]`. |
| `id` | [`Id`](#type-id) | yes |  | Unique among notifiers in every file. |
| `max_in_flight` | integer, ≥ 1 |  | `2` | The most deliveries this notifier sends at once. More wait for a free slot, so a storm of alerts never floods the service. |
| `name` | string |  |  | Display name; the id is used when it is absent. |
| `ntfy` | [table](#table-ntfysettings) |  |  | `[notifier.ntfy]`. |
| `pushover` | [table](#table-pushoversettings) |  |  | `[notifier.pushover]`. |
| `slack` | [table](#table-slacksettings) |  |  | `[notifier.slack]`. |
| `telegram` | [table](#table-telegramsettings) |  |  | `[notifier.telegram]`. |
| `type` | [`NotifierType`](#type-notifiertype) | yes |  | Which service the notifier posts to; its settings go in the table of the same name. |
| `webhook` | [table](#table-webhooksettings) |  |  | `[notifier.webhook]`. |

<a id="table-alertmanagersettings"></a>

#### `[notifier.alertmanager]`

`[notifier.alertmanager]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `bearer_token` | [`Secret`](#type-secret) |  |  | Sent as `Authorization: Bearer ...`. Secret. |
| `url` | string | yes |  | The Alertmanager base URL, such as `http://alertmanager:9093`. |

<a id="table-discordsettings"></a>

#### `[notifier.discord]`

`[notifier.discord]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `webhook_url` | [`Secret`](#type-secret) | yes |  | The channel webhook URL. Secret. |

<a id="table-emailsettings"></a>

#### `[notifier.email]`

`[notifier.email]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `to` | array of string | yes |  | Recipient addresses. |

<a id="table-ntfysettings"></a>

#### `[notifier.ntfy]`

`[notifier.ntfy]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `token` | [`Secret`](#type-secret) |  |  | An access token, for a protected topic. Secret. |
| `url` | string | yes |  | The topic URL, such as `https://ntfy.sh/my-alerts`. |

<a id="table-pushoversettings"></a>

#### `[notifier.pushover]`

`[notifier.pushover]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `api_token` | [`Secret`](#type-secret) | yes |  | The application's API token. Secret. |
| `degraded_priority` | integer, -128–127 |  | `0` | The priority of an alert that is degraded. |
| `down_priority` | integer, -128–127 |  | `1` | The Pushover priority, -2 to 2, of an alert that is down. 2 is an emergency, repeated until acknowledged and cancelled by the resolve. |
| `resolved_priority` | integer, -128–127 |  | `0` | The priority of a resolve. |
| `user_key` | [`Secret`](#type-secret) | yes |  | The user or group key alerts go to. Secret. |

<a id="table-slacksettings"></a>

#### `[notifier.slack]`

`[notifier.slack]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `connection` | [`Id`](#type-id) |  |  | The name of a channel connected on the admin page's Slack tab, with `[slack]`. A name not connected yet is a warning from `tocsin config check`; an alert sent to it fails loudly. |
| `webhook_url` | [`Secret`](#type-secret) |  |  | An incoming webhook URL you made yourself. Secret. Give this or `connection`, not both. |

<a id="table-telegramsettings"></a>

#### `[notifier.telegram]`

`[notifier.telegram]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `bot_token` | [`Secret`](#type-secret) | yes |  | The bot's token. Secret. |
| `chat_id` | string | yes |  | The chat alerts are sent to. |

<a id="table-webhooksettings"></a>

#### `[notifier.webhook]`

`[notifier.webhook]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `bearer_token` | [`Secret`](#type-secret) |  |  | Sent as `Authorization: Bearer ...`. Secret. |
| `signing_secret` | [`Secret`](#type-secret) | yes |  | The key each body is signed with: HMAC-SHA256 over the timestamp and the body, sent in `X-Tocsin-Signature`. Secret. |
| `url` | string | yes |  | The URL alerts are posted to as JSON. |

<a id="table-page"></a>

### `[page]`

`[page]`: how the public status page looks, its title, logo, links, the order of its groups and components, and the time zone it shows times in.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `accent` | [`Colour`](#type-colour) |  |  | The colour of links, buttons and the maintenance status, in hex such as `"#0b5cad"`. It must reach WCAG AA contrast (4.5:1) on the light scheme's page background and surfaces. Unless `accent_dark` is set, the dark scheme uses it mixed toward white until it reaches 4.5:1 on the dark backgrounds. Unset, the page uses its own blue. |
| `accent_dark` | [`Colour`](#type-colour) |  |  | The accent in the dark colour scheme, in hex. It must reach 4.5:1 on the dark scheme's page background and surfaces. Unset, it is derived from `accent`. |
| `components` | array of [`Id`](#type-id) |  |  | The order components are shown in within their group, by id. Components not listed follow in the order they are defined. |
| `description` | string |  |  | A line shown under the title. |
| `groups` | array of [`Id`](#type-id) |  |  | The order groups are shown in, by id. Groups not listed follow in the order they are defined. |
| `homepage_url` | string |  |  | Where the page's logo and title link to. |
| `link` | [array of tables](#table-link) |  |  | Links in the page's header, such as the main site or a support address, each a `[[page.link]]` table. |
| `logo` | [table](#table-logo) |  |  | The logo shown before the title, as `[page.logo]`. |
| `timezone` | string |  | `"UTC"` | The time zone the page shows times in: an IANA name such as `"Europe/Berlin"`. The uptime bars' days are UTC days regardless. |
| `title` | string |  | `"Status"` | The page title. |

<a id="table-logo"></a>

#### `[page.logo]`

`[page.logo]`: an image shown before the page title.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `alt` | string | yes |  | The image's text alternative. Leave it empty when the title beside the logo already says what it shows. |
| `url` | string | yes |  | A path on this site, such as `"/brand/logo.svg"`, that the proxy in front of tocsin serves. The page's content security policy loads images from its own origin only, so a URL on another host would not show. |

<a id="table-project"></a>

### `[[project]]`

`[[project]]`: something whose releases and deploys the release log records. CI pushes them; tocsin never polls.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `components` | array of [`Id`](#type-id) |  |  | The `[[component]]`s this project's deploys belong to: a timeline filtered to one of them shows them. |
| `description` | string |  |  | A line shown with the project. |
| `id` | [`Id`](#type-id) | yes |  | Unique among projects in every file. |
| `name` | string | yes |  | Display name. |
| `public` | boolean |  | `true` | Whether `/releases`, its feed and the unauthenticated API show the project's releases. A non-public project's releases are seen only with a `status:read` token or on the admin timeline. |
| `url` | string |  |  | The project's homepage or repository. |

<a id="table-route"></a>

### `[[route]]`

`[[route]]`: sends alerts matching its filters to its notifiers. Empty filters match everything.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `checks` | array of [`Id`](#type-id) |  |  | Only alerts for these checks. |
| `components` | array of [`Id`](#type-id) |  |  | Only alerts for these components. |
| `groups` | array of [`Id`](#type-id) |  |  | Only alerts for components in these groups. |
| `heartbeats` | array of [`Id`](#type-id) |  |  | Only alerts for these heartbeats. |
| `id` | [`Id`](#type-id) | yes |  | Unique among routes in every file. |
| `notifiers` | array of [`Id`](#type-id) | yes |  | The `[[notifier]]`s that receive matching alerts. |
| `notify_on` | [`FailureState`](#type-failurestate) |  | `"down"` | The least severe state that alerts: `degraded` alerts on degraded and down, `down` on down only. |
| `repeat` | [`Duration`](#type-duration) |  | `"4h"` | How often to send an alert again while it is still firing; `"0s"` never repeats. Alertmanager notifiers re-post every minute whatever this says, so Alertmanager never times the alert out. |

<a id="table-server"></a>

### `[server]`

`[server]`.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `auth` | [table](#table-auth) |  |  | How the admin page authenticates people. |
| `data_dir` | string |  |  | The folder holding the SQLite database. `tocsin serve --data` overrides it. |
| `email` | [table](#table-serveremail) |  |  | Sender settings for every email tocsin sends. |
| `listen` | string |  | `"127.0.0.1:8080"` | The address the web server listens on, as `ip:port`: `127.0.0.1:8080`, `0.0.0.0:8080` or `[::]:8080`. Hostnames are not accepted. |
| `max_concurrent_checks` | integer, ≥ 1 |  | `64` | The most checks that may run at once across the node. Checks due while the limit is reached wait for a free slot. |
| `max_silence` | [`Duration`](#type-duration) |  | `"1d"` | The longest a silence may last: a silence's expiry, given as `expires_at` or `expires_in`, must be at most this far ahead when it is made. Nothing extends a silence, so a longer one is a new one, made by someone. |
| `node_id` | string |  |  | This node's name in every result it records. Defaults to the hostname. |
| `public_url` | string |  |  | The URL people reach the status page at, used in links and emails. |
| `retention_days` | integer |  | `90` | Days of check results kept. |

<a id="table-auth"></a>

#### `[server.auth]`

`[server.auth]`: how the admin page at `/admin/` learns who is asking.

tocsin never handles passwords. It sits behind an identity-aware proxy and believes only what that proxy can prove: a signed Cloudflare Access token, or a header from a peer address you trust. Without this table, or with `mode = "none"`, the admin page does not exist.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `allowed_domains` | array of string |  |  | Email domains allowed in, matched exactly (`example.com` does not admit `team.example.com`). |
| `allowed_emails` | array of string |  |  | Email addresses allowed in. With neither this nor `allowed_domains`, everyone the proxy lets through is allowed. |
| `audience` | string |  |  | `cloudflare_access` only: the Access application's audience (AUD) tag. |
| `header` | string |  |  | `trusted_header` only: the request header carrying the email address. Defaults to `X-Forwarded-Email`. |
| `mode` | [`AuthMode`](#type-authmode) |  | `"none"` | How identities are proven. `none` turns the admin page off. |
| `team_domain` | string |  |  | `cloudflare_access` only: the Zero Trust team, as `name` or `name.cloudflareaccess.com`. |
| `trusted_proxies` | array of [`Cidr`](#type-cidr) |  |  | `trusted_header` only: the networks the proxy connects from. The header is believed only when the TCP peer is inside one of them; `X-Forwarded-For` is never consulted. |

<a id="table-serveremail"></a>

#### `[server.email]`

`[server.email]`. Required, with `from`, `reply_to` and `smtp_url`, whenever an email notifier exists.

Every email tocsin sends can be answered: neither address may be a `noreply`, `no-reply` or `donotreply` address.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `from` | string |  |  | The From address, as `address` or `Name <address>`. |
| `reply_to` | string |  |  | The Reply-To address: a mailbox a person reads. |
| `smtp_url` | [`Secret`](#type-secret) |  |  | The SMTP server, as `smtp://user:password@host:587` or `smtps://...`. Secret. |

<a id="table-slack"></a>

### `[slack]`

`[slack]`: your own Slack app, which people approve on Slack's consent screen to connect a channel. Each connection is an incoming webhook, kept in the database rather than in configuration.

With it, the admin page can connect channels for `[[notifier]]`s to name with `connection`, and, with `[subscribers]`, visitors can add status updates to their own Slack. `tocsin slack setup` prints the link that creates the app. It needs an `https` `[server] public_url`, which Slack sends people back to. Like `[server]`, it is read only at startup.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `app_name` | string |  |  | The app's name, as Slack shows it on the consent screen and beside each message, at most 35 characters. Used only by `tocsin slack setup`; defaults to the page's title. |
| `client_id` | [`Secret`](#type-secret) | yes |  | The app's Client ID, from its Basic Information page. Secret. |
| `client_secret` | [`Secret`](#type-secret) | yes |  | The app's Client Secret, from the same page. Secret. |

<a id="table-subscribers"></a>

### `[subscribers]`

`[subscribers]`: lets people subscribe by email to incidents on the components they choose and to the releases of public projects. Without this table the page shows no form and the subscriber API answers 404.

It needs `[server] public_url`, for the links in each email, and a complete `[server.email]` to send from. Like `[server]`, it is read only at startup.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `client_ip_header` | string |  |  | A header your proxy sets to the client's IP address, such as `CF-Connecting-IP`. It is believed only when the TCP peer is inside `trusted_proxies`; otherwise the peer's own address is the client's. |
| `confirm_within` | [`Duration`](#type-duration) |  | `"2d"` | How long a confirmation link works. An address not confirmed in time is forgotten. |
| `rate_window` | [`Duration`](#type-duration) |  | `"1h"` | The window both sign-up limits count over. |
| `signups_per_email` | integer |  | `3` | Sign-ups for one email address per `rate_window`, from anywhere. Past it, a sign-up is refused with 429 and `Retry-After`. |
| `signups_per_ip` | integer |  | `10` | Sign-ups one client IP address may make per `rate_window`. Past it, a sign-up is refused with 429 and `Retry-After`. |
| `trusted_proxies` | array of [`Cidr`](#type-cidr) |  |  | The networks your proxy connects from. Required with `client_ip_header`. |
| `turnstile` | [table](#table-turnstile) |  |  | Cloudflare Turnstile on the sign-up form, as `[subscribers.turnstile]`. With it, signing up needs JavaScript. |

<a id="table-turnstile"></a>

#### `[subscribers.turnstile]`

`[subscribers.turnstile]`: a Cloudflare Turnstile widget every sign-up must pass.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `secret_key` | [`Secret`](#type-secret) | yes |  | The widget's secret key, which tocsin sends to Cloudflare to check each answer. Secret. |
| `site_key` | string | yes |  | The widget's site key, which the page shows. |

<a id="table-telemetry"></a>

### `[telemetry]`

`[telemetry]`: where tocsin sends its own telemetry. Without it, it sends none. Like `[server]`, it is read only at startup.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `otlp` | [table](#table-otlp) |  |  | `[telemetry.otlp]`: push the checks' metrics over OTLP. The table turns the export on, even empty: each key it leaves out is taken from the standard `OTEL_EXPORTER_OTLP_*` environment variables, and a key it sets wins over them. |

<a id="table-otlp"></a>

#### `[telemetry.otlp]`

`[telemetry.otlp]`: an OTLP metrics exporter.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `endpoint` | string |  |  | The collector's base URL, such as `"http://otel-collector:4318"`. Over `http/protobuf` the metrics go to its `/v1/metrics`. Without it, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (used as is), then `OTEL_EXPORTER_OTLP_ENDPOINT`, then `http://localhost:4318` (`http://localhost:4317` over `grpc`). |
| `headers` | table of [`Text`](#type-text) |  |  | Headers sent with each export, such as an API key. A value may be an `${ENV:NAME}` secret, which is never logged. They are added to those in `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_EXPORTER_OTLP_METRICS_HEADERS`; a header named in both is sent with the value given here. |
| `interval` | [`Duration`](#type-duration) |  |  | How often the metrics are sent. Without it, `OTEL_METRIC_EXPORT_INTERVAL` (milliseconds), then 60s. |
| `protocol` | [`OtlpProtocol`](#type-otlpprotocol) |  |  | `"http/protobuf"` or `"grpc"`. Without it, `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, then `OTEL_EXPORTER_OTLP_PROTOCOL`, then `"http/protobuf"`. |
| `resource` | table of string |  |  | Resource attributes added to those tocsin sets, such as `{ "deployment.environment.name" = "production" }`. They win over `OTEL_RESOURCE_ATTRIBUTES`, and `service.name` here wins over `OTEL_SERVICE_NAME` and the default `"tocsin"`. `service.version` and `service.instance.id` (the `node_id`) are tocsin's own and cannot be set. |
| `timeout` | [`Duration`](#type-duration) |  |  | How long one export may take before it is abandoned and counted as failed. Without it, `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`, then `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds), then 10s or `interval`, whichever is shorter. It may not be longer than `interval`. |

<a id="table-token"></a>

### `[[token]]`

`[[token]]`: a bearer token for the write API.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `description` | string |  |  | Who or what uses the token. |
| `id` | [`Id`](#type-id) | yes |  | Unique among tokens in every file; logged with each write. |
| `scopes` | array of [`TokenScope`](#type-tokenscope) | yes |  | What the token may write. |
| `secret` | [`Secret`](#type-secret) | yes |  | The token's value. Secret. |

<a id="table-topology"></a>

### `[topology]`

`[topology]`: the known nodes of the map that `topology_node` refers to. Without it, any well-formed id is allowed.

The list is usually generated from the map itself into a file of its own in the configuration folder, so a node added to the map is allowed on the next reload.

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `nodes` | array of [`TopologyNode`](#type-topologynode) |  | `[]` | Every node id a `topology_node` may name. |

## Value types

Types a key's **Type** column links to: strings with a fixed form, and the values a choice accepts.

<a id="type-authmode"></a>

### `AuthMode`

How `Auth` proves who is asking.

| Value | Meaning |
|-------|---------|
| `"none"` | No admin page. |
| `"cloudflare_access"` | Cloudflare Access: verify the signed `Cf-Access-Jwt-Assertion` token against the team's published keys. |
| `"trusted_header"` | A proxy on a trusted network names the person in a header. |

<a id="type-bytesize"></a>

### `ByteSize`

A size such as "512B", "64KiB" or "1MiB".

A string matching `^[0-9]+(B|KiB|MiB)$`.

<a id="type-checktype"></a>

### `CheckType`

The probe a check runs.

| Value | Meaning |
|-------|---------|
| `"http"` | An HTTP or HTTPS request. |
| `"tcp"` | A TCP connection. |
| `"dns"` | A DNS lookup. |
| `"icmp"` | An ICMP echo (ping). |
| `"tls"` | A TLS handshake, inspecting the certificate. |

<a id="type-cidr"></a>

### `Cidr`

An IP network such as "10.0.0.0/8" or "fd00::/8", or a single address such as "192.0.2.7".

A string.

<a id="type-colour"></a>

### `Colour`

A colour in hex: "#0b5cad" or "#06c".

A string matching `^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$`.

<a id="type-componentrule"></a>

### `ComponentRule`

How a component's health is derived from the health of its checks.

| Value | Meaning |
|-------|---------|
| `"worst"` | The worst check decides: one check down makes the component down. |
| `"majority"` | The worst state that more than half of the checks share or exceed: one check down out of three leaves the component up. |

<a id="type-duration"></a>

### `Duration`

A duration such as "500ms", "30s", "5m", "1h30m" or "1d".

A string matching `^([0-9]+(ms|s|m|h|d))+$`.

<a id="type-failurestate"></a>

### `FailureState`

The state a failed assertion puts its check in.

| Value | Meaning |
|-------|---------|
| `"degraded"` | Working, but worse than it should be. |
| `"down"` | Not working. |

<a id="type-httpmethod"></a>

### `HttpMethod`

An HTTP request method.

| Value | Meaning |
|-------|---------|
| `"GET"` | `GET`. |
| `"HEAD"` | `HEAD`. |
| `"POST"` | `POST`. |
| `"PUT"` | `PUT`. |
| `"PATCH"` | `PATCH`. |
| `"DELETE"` | `DELETE`. |
| `"OPTIONS"` | `OPTIONS`. |

<a id="type-id"></a>

### `Id`

A slug: lowercase letters, digits and hyphens, starting with a letter or digit.

A string matching `^[a-z0-9][a-z0-9-]*$`.

<a id="type-notifiertype"></a>

### `NotifierType`

The service a notifier posts to.

| Value | Meaning |
|-------|---------|
| `"slack"` | A Slack incoming webhook. |
| `"pushover"` | Pushover. |
| `"email"` | Email over SMTP. |
| `"ntfy"` | An ntfy topic. |
| `"discord"` | A Discord webhook. |
| `"telegram"` | A Telegram bot. |
| `"webhook"` | A generic JSON webhook. |
| `"alertmanager"` | The Alertmanager v2 alerts API. |

<a id="type-otlpprotocol"></a>

### `OtlpProtocol`

How OTLP is carried.

| Value | Meaning |
|-------|---------|
| `"http/protobuf"` | Protobuf in an HTTP POST, to port 4318 by convention. |
| `"grpc"` | gRPC, to port 4317 by convention. |

<a id="type-probeclass"></a>

### `ProbeClass`

What kind of probe a check or heartbeat is, carried on its alerts as the `class` label.

| Value | Meaning |
|-------|---------|
| `"production"` | Watches something real; its alerts are real. |
| `"test"` | Exercises tocsin or a receiver; its alerts are not outages. |
| `"drill"` | A rehearsal of an outage, such as a paging drill. |
| `"canary"` | Watches a canary, which is expected to fail now and then. |

<a id="type-recordtype"></a>

### `RecordType`

A DNS record type.

| Value | Meaning |
|-------|---------|
| `"A"` | IPv4 address. |
| `"AAAA"` | IPv6 address. |
| `"CNAME"` | Canonical name. |
| `"MX"` | Mail exchanger. |
| `"NS"` | Name server. |
| `"TXT"` | Text. |
| `"SRV"` | Service locator. |
| `"CAA"` | Certification authority authorization. |

<a id="type-secret"></a>

### `Secret`

A secret, read from an environment variable: "${ENV:NAME}".

A string matching `^\$\{ENV:[A-Za-z_][A-Za-z0-9_]*\}$`.

<a id="type-text"></a>

### `Text`

A string, or a secret read from an environment variable: "${ENV:NAME}".

A string.

<a id="type-tokenscope"></a>

### `TokenScope`

What a token may do.

| Value | Meaning |
|-------|---------|
| `"heartbeat"` | Heartbeat pings. |
| `"incident"` | Incidents and maintenance: open, update, edit, publish and discard them. Includes everything `incident:draft` may do. |
| `"incident:draft"` | Draft incidents only: open an incident as a draft, and update, edit or discard a draft. Never publishes and never changes a published incident, so whoever holds it (an AI seat, a script) cannot put anything in front of the public or subscribers without a person. |
| `"release"` | Releases and deploys. |
| `"status:read"` | Reading what is not public in the read API: non-public components, checks and heartbeats on no component or a non-public one, non-public projects' releases and deploys, check targets, raw check results, and how the configuration last reloaded. The internal board behind proxy auth reads the same. |
| `"subscribers:admin"` | Listing, counting and removing email subscribers. |
| `"silence"` | Silences: make, list and end them. Every silence records the token or person that made it. |

<a id="type-topologynode"></a>

### `TopologyNode`

A node id in an external map of the estate: lowercase letters and digits joined by single `-`, `.`, `_`, `/` or `:`.

A string matching `^[a-z0-9]+([-._/:][a-z0-9]+)*$`.

