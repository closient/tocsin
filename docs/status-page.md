# The status page

What `tocsin serve` shows the public, with no sign-in. It reads exactly
what the anonymous [read API](api.md) does: the same component status, the
same incidents, and releases of public projects only. Its look is set by
[`[page]`](config.md#page).

## Pages

| Page | |
|---|---|
| `/` | The overall status banner; each group and its components with their status; a 90-day uptime bar per component; incidents and maintenance in progress; the five most recently resolved; the five latest releases, when a project is public. |
| `/incidents/<id>` | One incident or maintenance window: its components, every update newest first, and when it started and ended. |
| `/history` | Every incident and maintenance window, newest first, 25 to a page, with a link to older ones. |
| `/components/<id>` | One component: its status, its uptime bar, what is open against it, and its outages, incidents and deploys of the last 30 days. |

Times are shown in `[page] timezone`. Update messages are Markdown,
rendered by the same sanitizing renderer as the rest of tocsin. Every page
reads in full without JavaScript and follows the reader's light or dark
preference.

## Uptime bars

One cell per UTC day for the last 90 days, read from the daily rollups the
server recomputes every hour, so today's cell trails the live status by up
to an hour. A day's uptime for a check is the share of its results (or, for
a heartbeat, its time) that were not down, every node summed; degraded
counts as up. A component's day is the mean over its checks, and the cell
shows the worst health any of them saw: up, degraded, down, or no data,
each with its own fill pattern so none rests on colour alone. Hovering a
cell shows its day and figure, which a screen reader reads as text, and a
line under each bar sums it up. Percentages are floored, so a day with one
failure never reads 100%.

## Live updates

`/` loads two scripts from the binary: htmx and its server-sent events
extension, both vendored unmodified (see
[`crates/tocsin/static/README.md`](../crates/tocsin/static/README.md)). The
page follows [`/api/v1/events`](api.md#events) and re-fetches a section's
markup from `/fragments/<name>` when an event names a change to it: an
incident posted through the API is on every open page within a second or
two. Each section also re-fetches every 60 seconds (the uptime bars every
10 minutes) in case the stream drops, and all of them on `resync`. A
fragment is the same template the page includes, so a refreshed section is
what a reload would show.

The content security policy is unchanged: scripts and styles from the
page's own origin only, no inline script, and htmx is configured not to
evaluate any.

## Behind a CDN

Every response carries an `ETag` of its body and answers a matching
`If-None-Match` with 304; nothing in a page records when it was rendered,
so an unchanged page keeps its tag. Pages are sent with
`Cache-Control: public, max-age=0, s-maxage=15, stale-while-revalidate=30`,
so a CDN can serve a copy for 15 seconds and a stale one while it
refetches, and a browser always revalidates. Fragments are
`public, no-cache`: revalidated on every poll, a 304 when nothing changed.
The event stream is never cached and carries `X-Accel-Buffering: no`.

## Structured data

Each page carries JSON-LD: a `WebPage` for the page itself and an `Event`
for each incident or maintenance window it shows, with its start, end and
a link to its page.
