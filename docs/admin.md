# The admin page

`/admin/` lets a signed-in person open an incident, post updates and
resolve it from a phone. It is off until `[server.auth]` names an
authenticating proxy; with `mode = "none"` (the default) `/admin/` is a
404 like any unknown path.

tocsin does no sign-in of its own. A proxy in front of it signs people in
and tells tocsin who they are, in one of two ways:

- **`cloudflare_access`**: Cloudflare Access signs every request it lets
  through with a short-lived RS256 token in `Cf-Access-Jwt-Assertion`.
  tocsin checks the signature against the team's published keys and reads
  the `email` claim.
- **`trusted_header`**: a proxy you run (oauth2-proxy, Pomerium, Caddy with
  forward auth) puts the email address in a header. tocsin believes the
  header only from a connection whose socket address is in
  `trusted_proxies`.

`allowed_emails` and `allowed_domains` then narrow who may use the page.
The keys are in [config.md](config.md#serverauth).

The same sign-in guards the read-only [internal board](#the-internal-board)
at `/internal/`.

## The internal board

`/internal/` shows every check and heartbeat, including those on no
public component: the machines, certificates, heartbeats and vendor status
pages that do not belong on the public page. It replaces the Gatus
dashboard. It is read-only; checks are changed only in the config files.

Checks are grouped like the status page: each group's components in
order, then a last section, "Not on the status page", for checks and
heartbeats on no component. Each check shows:

- its health and how long it has held it;
- its type, interval, target and component;
- a latency sparkline of the last 24 hours, one point per half-hour,
  with a mark under each half-hour that saw a failure, and the same in
  words beneath it;
- its 90-day uptime bar, from the daily rollups;
- the last problem in the past 24 hours, and the newest result's
  assertions in a sentence;
- when it last ran;
- its `[[check.link]]` links, shown nowhere else.

A heartbeat shows its last ping, its next deadline (the last result plus
`interval` and `grace`) and the message its last ping carried, with its
`[[heartbeat.link]]` links.

`/internal/checks/<id>` is one check or heartbeat: the same card, every
assertion of the newest result with what it expected and got, the results
of the last 24 hours (newest 50), and its outages over 90 days with the
releases of the projects that list its component (every project's, for a
check on none).

Both pages read in full without JavaScript. With it, they refresh when
the board's own event stream, `/internal/events`, reports a check change,
and every minute besides.

The board answers **403** to every request that is not from a signed-in
person on the allow-lists, and also while `[server.auth]` is off: unlike
`/admin/`, which is then a 404, the board exists in every install and
says how to reach it. An unknown check is a 404 only after sign-in, so ids
cannot be probed from outside. Every response, refusals included, carries
`Cache-Control: private, no-store` and `X-Robots-Tag: noindex`, and the
pages carry no structured data. Nothing public links to the board: not
`/`, `/llms.txt`, the OpenAPI document or the feeds. tocsin serves no
sitemap.

`/internal/events` is the board's own stream, behind the same guard: it
carries every check change, including those of checks on no component.
The public stream, `/api/v1/events`, carries only changes of checks and
heartbeats on a component, as the anonymous `GET /api/v1/checks` lists
only those; see [api.md](api.md#public-and-internal-checks).

## Setting it up

### Cloudflare Access

1. In Zero Trust, add a self-hosted application for the status host with
   the path `admin`, and a policy for the people who run incidents.
2. Copy the application's **Application Audience (AUD) Tag**.
3. Configure tocsin. `public_url` is required whenever `[server.auth]` is
   on: forms are accepted only from that origin.

   ```toml
   [server]
   public_url = "https://status.example.com"

   [server.auth]
   mode = "cloudflare_access"
   team_domain = "example"            # example.cloudflareaccess.com
   audience = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2"
   allowed_domains = ["example.com"]  # optional, on top of the Access policy
   ```

tocsin fetches the keys from
`https://<team>.cloudflareaccess.com/cdn-cgi/access/certs` on the first
request, again once they are an hour old, and when a token names a key it
does not have (at most once a minute). One background fetch runs at a
time and no request waits on a refresh: while it runs, requests keep using
the keys already held.

- Until the first fetch succeeds every admin request is refused, and
  fetches back off from one second, doubling to once a minute.
- After that, a failed fetch keeps the last keys for one more hour. Keys
  older than two hours are not trusted at all: every admin request is
  refused, and each one logs an error, until a fetch succeeds.

Either way the server log says why.

The `Cf-Access-Authenticated-User-Email` header and the `CF_Authorization`
cookie are never read: anything that reaches tocsin without going through
Access, such as a request to the origin's address, can send them.

### A trusted header

```toml
[server]
public_url = "https://status.example.com"

[server.auth]
mode = "trusted_header"
header = "X-Forwarded-Email"          # the default
trusted_proxies = ["10.0.0.5/32"]     # the proxy's address as tocsin sees it
allowed_emails = ["alice@example.com"]
```

The proxy must strip the header from incoming requests and set it itself.
tocsin checks the TCP peer, never `X-Forwarded-For`, `X-Real-IP` or
`Forwarded`, so only the proxy's own connections count. On a dual-stack
listener an IPv4 proxy connects as `::ffff:10.0.0.5`; it matches either
`10.0.0.5/32` or `::ffff:10.0.0.5/128`. List the narrowest
networks that hold the proxy: anything else that can connect from inside
them can name any person.

## Subscribers

With [`[subscribers]`](subscribers.md) on, `/admin/subscribers`, linked
from `/admin/`, lists every address, 50 to a page in the order they signed
up, with whether it has confirmed and what it chose, the confirmed and
unconfirmed totals, and a button that removes one. Removing deletes the
address, its choices and any email still queued for it. The button is a
form like the others here, under the same origin and token checks.

## What it protects against

| Threat | Defence |
|--------|---------|
| A request that skipped the proxy names a person. | Access mode: only a token signed by the team's keys counts; the email header and cookie are ignored. Header mode: the header is ignored unless the socket peer is in `trusted_proxies`. |
| A forged or altered Access token. | RS256 only; `none`, HMAC and other algorithms are refused before any key is looked up. The signature, `iss` (the team), `aud` (this application), `exp` and `nbf` are all checked, with 30 seconds of clock leeway. |
| A token minted for another Access application of the same team. | `aud` must contain the configured audience. |
| A flood of tokens naming unknown keys. | Keys are refetched at most once a minute, by one task at a time. |
| Access unreachable, so the keys cannot be refreshed. | The last keys are kept for an hour past their age, then refused: a key Cloudflare has revoked stops working within two hours. |
| Someone the proxy admits who should not run incidents. | `allowed_emails` / `allowed_domains`: 403. |
| Anyone reading the internal board's targets, errors and links. | Every `/internal/` route takes the same sign-in as `/admin/` and answers 403 without it, including when `[server.auth]` is off. |
| A shared cache or search engine keeping a copy of the internal board. | `Cache-Control: private, no-store` and `X-Robots-Tag: noindex` on every `/internal/` response, refusals included. |
| A page on another site submits a form with the person's session. | Two checks, both required: `Origin` (or `Referer`) must be the origin of `[server] public_url` (scheme, host and port; the request's `Host` is never consulted) and `Sec-Fetch-Site`, when sent, `same-origin`; and a token bound to the person, `HMAC-SHA256` with a per-process key, valid for 12 hours, sent as the `csrf` form field. |
| Script in an incident's title or message. | askama escapes every value; messages are rendered by pulldown-cmark and cleaned by ammonia, and only that output is marked safe. The Content-Security-Policy allows only this site's own script and stylesheet, with no inline code; admin pages load no script at all. |
| Framing, sniffing, caching of admin pages. | `X-Frame-Options: DENY`, `frame-ancestors 'none'`, `nosniff`, `Cache-Control: no-store`. |

Not covered: anyone who can reach tocsin from inside `trusted_proxies`, or
who holds the Access signing keys, can act as anyone. The CSRF key is
random per process, so a restart signs everyone's open pages out of their
forms: they reload and carry on. Several nodes behind one name each have
their own key, so the proxy must send a person to the same node.

Writes go through the same functions as the JSON API
(`http::incidents::open` and `append`): the same validation, storage and
events. The author recorded is `{"kind": "person", "id": "<email>"}`.

## Adding a page

The admin pages and the release pages (`crates/tocsin/src/web/releases.rs`)
are server-rendered this way, and the status page will follow the same
pattern. The pieces live in
`crates/tocsin/src/web/`:

- **Templates** are askama files in `crates/tocsin/templates/`. A page is
  a struct with `#[derive(Template)]`, a `layout: page::Layout` field and
  `{% extends "base.html" %}`. Use `Layout::public(title)` for a page
  anyone may see and `Layout::admin(title, &admin)` for a signed-in one;
  return it with `page::render(status, &page)`. Errors are
  `page::HtmlError`, which renders `error.html`.
- **Escaping**: askama escapes every value. The only `|safe` allowed is
  the output of `markdown::render`.
- **Assets** live in `crates/tocsin/static/`. Add the file to `FILES` in
  `assets.rs` and link it with `assets::url("name")`; it is served at
  `/static/<hash>/<name>` and cached for a year. No page loads anything
  from another origin: htmx is vendored for the status page's live
  updates, and the CSP forbids the rest.
- **Identity**: a handler that needs a signed-in person takes
  `identity::Admin`; one that writes takes `identity::AdminForm`, which
  adds the Origin and CSRF checks and parses the form:

  ```rust
  async fn page(State(state): State<AppState>, admin: Admin) -> Result<Response, HtmlError>
  async fn write(State(state): State<AppState>, AdminForm { admin, fields }: AdminForm) -> Result<Response, HtmlError>
  ```

  Both answer 404 while `[server.auth]` is off, 401 to someone the proxy
  did not vouch for and 403 to someone not on the allow-lists. A
  read-only page for operators takes `identity::Operator` instead, which
  runs the same check and answers every refusal, `[server.auth]` being
  off included, with one 403. A form
  includes `<input type="hidden" name="csrf" value="{{ csrf }}">`, from
  `admin.csrf_token()`.
- **Forms are plain HTML forms**, without htmx: a write answers `303 See
  Other` to the page it changed, or re-renders the form with `422` and the
  errors. Give every POST path a GET that redirects to its page, so a
  reload of the address bar lands on a page.
- **Routes** go in the page's module and are merged in `web::router`,
  which adds the security headers to every response.
- **Tests** go in `web/tests.rs`, driving the router with `tower::oneshot`
  or a real socket.

htmx is pinned in `crates/tocsin/static/README.md`; upgrading it means
replacing the file and the checksum `vendored_htmx_is_the_pinned_release`
asserts.
