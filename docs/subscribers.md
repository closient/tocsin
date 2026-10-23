# Email subscribers

People can ask the status page to email them when an incident opens or is
updated on the components they choose, and when a public project they
follow publishes a release. Every address is confirmed before it receives
anything but the confirmation, every email can be answered, and every
email can be left in one click.

The feature is off until `[subscribers]` is in the configuration. Off, `/`
shows no form, `/subscribe/…` and `/admin/subscribers` are 404 like any
unknown path, and so is every `/api/v1/subscribers` route.

## Turning it on

```toml
[server]
public_url = "https://status.example.com"

[server.email]
from = "Example Status <status@example.com>"
reply_to = "support@example.com"
smtp_url = "${ENV:SMTP_URL}"

[subscribers]
# Every key is optional; these are the defaults.
signups_per_ip = 10
signups_per_email = 3
rate_window = "1h"
confirm_within = "2d"
```

`[subscribers]` needs `public_url`, for the links in each email, and a
complete `[server.email]`: the same sender, Reply-To and SMTP server the
operator email notifier uses, under the same rule that rejects any
`noreply` address. The keys are in
[config-reference.md](config-reference.md#table-subscribers). Like
`[server]`, the table is read at startup: a reload that changes it keeps
the running one, logs a warning, and applies everything else.

## Signing up

`/` shows a short form: an address, a box for every component and one for
every public project, and a link to `/subscribe`, where each component and
project can be chosen on its own. It is a plain form post, so it works with JavaScript off. Next to
it, the page links the [Atom feeds](status-page.md#feeds) of incidents
and releases, for anyone who would rather not give an address.

`POST /api/v1/subscribers` does the same from code:

```sh
curl -X POST https://status.example.com/api/v1/subscribers \
  -H 'Content-Type: application/json' \
  -d '{"email": "someone@example.com", "components": ["api"], "projects": ["*"]}'
```

`components` defaults to `["*"]` and `projects` to none. `*` is every
component, or every public project, including ones added later. A
component that does not exist, or a project that does not exist or is not
public, is a 422 naming the field.

The answer is the same 202 whether the address is new, is waiting for its
confirmation or is already subscribed, so it cannot be used to learn who
subscribes. Only the email differs: a new or waiting address gets a fresh
confirmation link (for a waiting one, the newer choices replace the
earlier ones and the earlier link stops working), and a confirmed one
gets a note with its manage link instead, its choices unchanged.

### Limits

Two limits count sign-ups over `rate_window`: `signups_per_ip` from one
client address and `signups_per_email` for one email address from
anywhere. Past either, the form and the API answer 429 with `Retry-After`
in seconds; nothing is dropped silently. The counts are in memory and
start again on restart.

Behind a proxy, every request comes from the proxy's address, so one
person could use up the limit for everyone. Set `client_ip_header` to the
header the proxy puts the client's address in, and `trusted_proxies` to
the networks the proxy connects from; the header is believed only from
those.

```toml
[subscribers]
client_ip_header = "CF-Connecting-IP"
trusted_proxies = ["173.245.48.0/20", "103.21.244.0/22"]
```

### Cloudflare Turnstile

```toml
[subscribers.turnstile]
site_key = "0x4AAAAAAA..."
secret_key = "${ENV:TURNSTILE_SECRET}"
```

With Turnstile on, every sign-up must carry an answer that Cloudflare
accepts: `cf-turnstile-response` from the form, `turnstile` in the API
body. The widget is a script, so signing up then needs JavaScript. `/`
keeps its strict content security policy and shows a link to
`/subscribe` in place of the form; only `/subscribe` loads the widget,
and its policy adds `https://challenges.cloudflare.com` to `script-src`
and `frame-src`. A missing or rejected answer is a 422; an
answer that could not be checked is a 503.

## Confirming

The confirmation email links to `/subscribe/confirm/<token>`, which shows
a button that confirms. Opening the link changes nothing, so a mail
scanner that follows it does not subscribe anyone. The link works for
`confirm_within`; after that it is a 404 that says it has expired, and the
address is forgotten within the hour.

Until it is confirmed, an address receives nothing but confirmations: no
delivery is ever queued for it, and the postman checks again before each
send.

## Managing and leaving

Every email to a confirmed address ends with two links:

- `/subscribe/manage/<token>` shows the address and its choices, and
  saves new ones.
- `/subscribe/unsubscribe/<token>` asks once, then removes the address.

Each message also carries `List-Unsubscribe` with the unsubscribe link and
`List-Unsubscribe-Post: List-Unsubscribe=One-Click` (RFC 8058), so a mail
client's own unsubscribe button posts to it directly. The post needs no
sign-in, cookie or origin. The address is deleted in that request, with
every delivery still queued for it, so nothing more is sent, not even a
message already queued. A second post finds nothing and says so.

A manage token is the subscriber's id and a 256-bit random secret,
compared in constant time. A wrong secret is a 404 that says nothing about
the subscriber.

## What is sent

| Event | Who | Suppressed by |
|---|---|---|
| An incident or maintenance window is opened | Subscribers to any of its components, or to `*`. One naming no component reaches every subscriber to incidents. | |
| An update is posted | The same | `"notify": false` in the update, or `tocsin incident update --quiet`: for a typo fix or a minor note |
| A release is published | Subscribers to its project, or to `*`, if the project is `public` | |

Edits (`PATCH /api/v1/incidents/{id}`) send nothing. A version reaching a
second environment is not announced again.

Each email is plain text from `[server.email] from`, with Reply-To set to
`reply_to`, so a reply reaches a person. It names the incident or release,
links its page, and ends with the manage and unsubscribe links.

## Delivery

An announcement is written to the store when the incident, update or
release is, under a unique key (`incident:7`, `incident:7:update:12`,
`release:api:1.4.0`), and the postman, a task inside `tocsin serve`,
expands it into one delivery per matching confirmed subscriber. The
delivery log is unique on (subscriber, event), so an event reaches an
address at most once, across restarts and however many times it is
queued.

A delivery is marked `sending` in its own committed write before the SMTP
server sees it. If the process stops mid-send, the next start marks that
delivery failed rather than sending it again: the server may already have
taken it, and a missed email is better than a duplicate. A send the server
refuses is retried, backing off from 5 seconds to 10 minutes, and given up
after 12 attempts. Finished deliveries are kept for 30 days. The tables
are in [storage.md](storage.md#subscriber-subscription-subscriber_event-and-subscriber_delivery).

## For operators

`/admin/subscribers`, linked from the admin page, lists every address with
its choices and whether it has confirmed, the totals, and a button to
remove each one. See [admin.md](admin.md).

The same is in the API, with a token that has the `subscribers:admin`
scope:

| Method and path | Success |
|---|---|
| `GET /api/v1/subscribers?after=&limit=` | 200, `{subscribers, next_after, totals: {confirmed, unconfirmed}}`, in id order |
| `DELETE /api/v1/subscribers/{id}` | 204; a second time, 404 |

```toml
[[token]]
id = "support-desk"
secret = "${ENV:SUBSCRIBERS_TOKEN}"
scopes = ["subscribers:admin"]
```

Logs never hold a whole address: they show its first letter and its
domain (`s***@example.com`).
