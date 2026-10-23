# Releases and the timeline

The release log records what shipped and when. CI pushes each release and
each deploy; tocsin never polls a forge. Releases sit on the same timeline
as outages and incidents, so "what changed right before this broke" is one
read: `GET /api/v1/timeline`, or `/admin/timeline` for a signed-in
operator.

## The model

A release belongs to a configured [`[[project]]`](config.md#project):

```toml
[[project]]
id = "api"
name = "API server"
components = ["api"]   # its deploys show on these components' timelines
public = true          # the default; false hides it from anonymous readers
```

| Field | |
|---|---|
| `id` | Assigned by tocsin, never reused. |
| `project` | The `[[project]]` id. An id the configuration does not define is refused with a 422 that names it. |
| `version` | 1 to 64 characters from `A-Z a-z 0-9 . _ + -`, starting with a letter or digit: `v1.4.0`, `2026.10.23+5`, `1.2.3-rc.1`. It appears in URLs, so `/`, `..`, `.`, spaces and non-ASCII (`vé`) are refused, in the body and in the path. |
| `environment` | Defaults to `production`. Same characters as `version`, 1 to 32. |
| `title` | One line, at most 200 characters; defaults to the version. |
| `notes` | Markdown, at most 64 KiB (65,536 bytes). |
| `url` | Optional `http` or `https` link, such as the GitHub release; at most 2,048 bytes. |
| `released_at` | When it was published. `null` on a deploy stamped before its publish. |
| `deployed_at` | When it was last deployed, if it has been. |
| `created_at`, `created_by` | When tocsin first heard of it, and the token (or person) that wrote it. |
| `updated_at`, `updated_by` | The last write and its author. |

### One row per project, version and environment

`(project, version, environment)` is the idempotency key: a `UNIQUE`
constraint in the database, and every write is one `INSERT … ON CONFLICT DO
UPDATE` in one transaction. A CI job that retries, or a release published
again with corrected notes, updates the row; it never makes a second one.

- **Publishing again** replaces `title`, `notes` and `url`. `released_at`
  keeps its recorded value when omitted, may move later, and is refused
  (422) if earlier; a database trigger holds the same rule. A
  `deployed_at` in the body replaces the recorded one; omitted, the
  recorded one stays.
- **Each environment is its own release.** `v1.4.0` to `staging` and to
  `production` are two rows with their own deploy times.
- **A deploy stamped before the publish creates a stub**: empty title and
  notes, `released_at` null. Pipelines that deploy and then publish, or
  whose two jobs race, lose nothing; the publish fills the stub in and
  keeps its `deployed_at`. A stub is on the timeline (as a deploy) but not
  in the release log or the public pages until it is published.
- The first write answers **201**, every later one **200**, with the
  release either way.

## Tokens

Writes need a `[[token]]` with the `release` scope. Reads need none for
public projects; a token with `status:read` also sees projects marked
`public = false`.

```toml
[[token]]
id = "deploys"                          # recorded as created_by / updated_by
secret = "${ENV:TOCSIN_RELEASE_TOKEN}"
scopes = ["release"]

[[token]]
id = "dashboards"
secret = "${ENV:TOCSIN_READ_TOKEN}"
scopes = ["status:read"]
```

A read request with no `Authorization` header is anonymous. One with a
header must carry a known token (else 401) with `status:read` (else 403):
a wrong token is refused, not quietly treated as anonymous.

## Visibility

One function decides what a reader may see:
`crate::http::releases::visible_projects`. Without `status:read` it is the
configured projects marked `public`; with it, every project. Every read of
releases (the list API, the timeline API, `/releases`, `/releases.atom`,
`/releases/<project>/<version>`) passes that answer to the store, which
filters in SQL. A release whose project is no longer configured is never
public. Asking for a non-public project anonymously looks exactly like
asking for one that does not exist: an empty list from the API, a 404 from
the pages.

Outages and incidents on the timeline are not filtered: the status page
shows them to everyone already.

## HTTP API

| Method and path | Token | Body | Success |
|---|---|---|---|
| `POST /api/v1/releases` | `release` | `{project, version, environment?, title?, notes?, url?, released_at?, deployed_at?}` | 201 created, 200 updated; the release |
| `POST /api/v1/releases/{project}/{version}/deployed` | `release` | optional `{environment?, deployed_at?}`; empty means production, now | 201 stub created, 200 stamped (the new time replaces the recorded one); the release |
| `GET /api/v1/releases?project=&since=&limit=` | none, or `status:read` | | 200, `{releases, truncated}`, newest `released_at` first |
| `GET /api/v1/timeline?component=&since=&limit=` | none, or `status:read` | | 200, `{since, events, truncated}`, oldest first |

- **Release list**: `limit` 1 to 200, default 50. `since` keeps releases
  published at or after it. Stubs are not listed.
- **Timeline**: outages (at their start), incidents and maintenance (at
  creation), deploys (at `deployed_at`) and releases not yet deployed (at
  `released_at`), read from one database snapshot. Ties on time order by
  kind (outage, incident, deploy, release), then id. `since` defaults to
  30 days ago and may reach back at most 366 days; `limit` is 1 to 500,
  default 100. `truncated` says more events fell in the window: ask again
  with `since` set to the last event's `at` (events at that instant repeat;
  skip them by `kind` and `id`). `component` keeps outages of its checks
  and heartbeats, incidents naming it, and deploys of projects that list
  it; an unknown component is a 422.

Errors are the JSON shape [incidents.md](incidents.md#errors) describes.
The release-specific ones:

| Status | `error` | When |
|---|---|---|
| 401 | `unauthorized` | A write with no token or an unknown one; a read with an unknown one. |
| 403 | `forbidden` | A write without `release`; a read with a token that lacks `status:read`. |
| 413 | `too_large` | `notes` over 64 KiB. (A body over 2 MiB is also 413.) |
| 422 | `invalid` | An unknown project or component (named in the message), a bad version or environment (in the body or the path), a title over 200 characters, `released_at` earlier than recorded, a `since` or `limit` out of range. |

### Events

Each successful write emits one event on the internal bus after its
transaction commits: `release_published` or `release_deployed`, carrying the
release and `public` (whether its project is public), so a subscriber that
reaches anonymous readers can skip private ones. Refused writes emit
nothing.

## Pages

These load no script and fit a phone.

| Page | |
|---|---|
| `/releases` | Published releases of public projects, newest first; `?project=` narrows to one. |
| `/releases/<project>/<version>` | One release: title, notes, and every environment it was published to with its deploy time. A stub (deployed, never published) is not shown. |
| `/releases.atom` | An Atom feed of the latest 20, with `?project=`. Links are absolute when `[server] public_url` is set. |
| `/admin/timeline` | Behind [admin sign-in](admin.md): the last 30 days of outages, incidents and deploys, newest first, every project included, with a component filter. When more than 500 events fall in the window it says so and shows the oldest 500. |

Notes are Markdown rendered by the same sanitizing renderer incidents use:
raw HTML is escaped, links are `http`, `https` or `mailto` only, and no
`<script>`, event handler or `javascript:` URL survives. Titles are
escaped as text.

## CLI

`tocsin release` is an HTTP client of a running server; `--url`/`TOCSIN_URL`
and `--token`/`TOCSIN_TOKEN` work as for [`tocsin incident`](incidents.md#cli).

```sh
export TOCSIN_URL=https://status.example.com TOCSIN_TOKEN=...

tocsin release publish --project api --version v1.4.0 --notes-file CHANGELOG-1.4.0.md \
  [--title "Faster search"] [--environment staging] \
  [--release-url https://github.com/acme/api/releases/tag/v1.4.0] \
  [--released-at 2026-10-23T14:05:00Z] [--deployed-at now]
tocsin release deployed --project api --version v1.4.0 [--environment staging] [--deployed-at now]
```

`--notes-file -` reads stdin. `--deployed-at` and `--released-at` take
`now` or RFC 3339. The link flag is `--release-url` because `--url` is the
server. `deployed` checks the project and version before sending, since
both go into the request path.

With `--json`, stdout is the release. Exit codes: 0 on success; 2 for
invalid input, including any 4xx, an unreadable notes file and a version
that is not URL-safe; 3 when the server cannot be reached or answers 5xx.
(1, `check_failed`, is not used by these commands.)

## From CI

### GitHub Actions

Publish when a GitHub release is published; stamp the deploy after the
deploy job. `curl` and `jq` are on every hosted runner, so nothing needs
installing:

```yaml
on:
  release:
    types: [published]

jobs:
  deploy:
    runs-on: ubuntu-latest
    env:
      TOCSIN_URL: https://status.example.com
      TOCSIN_TOKEN: ${{ secrets.TOCSIN_RELEASE_TOKEN }}
      VERSION: ${{ github.event.release.tag_name }}
    steps:
      - name: Record the release
        env:
          TITLE: ${{ github.event.release.name }}
          NOTES: ${{ github.event.release.body }}
          LINK: ${{ github.event.release.html_url }}
        run: |
          jq -n --arg version "$VERSION" --arg title "${TITLE:-$VERSION}" \
                --arg notes "$NOTES" --arg url "$LINK" \
            '{project: "api", version: $version, title: $title, notes: $notes, url: $url}' |
          curl --fail-with-body -sS -X POST "$TOCSIN_URL/api/v1/releases" \
            -H "Authorization: Bearer $TOCSIN_TOKEN" \
            -H "Content-Type: application/json" --data-binary @-

      - name: Deploy
        run: ./deploy.sh "$VERSION"

      - name: Record the deploy
        run: |
          curl --fail-with-body -sS -X POST \
            "$TOCSIN_URL/api/v1/releases/api/$VERSION/deployed" \
            -H "Authorization: Bearer $TOCSIN_TOKEN"
```

With the `tocsin` binary on the runner the two steps are
`tocsin release publish --project api --version "$VERSION" --notes-file notes.md`
and `tocsin release deployed --project api --version "$VERSION"`.

Both calls are idempotent, so re-running a failed workflow is safe, and the
order does not matter: if the deploy step runs first it leaves a stub the
publish fills in.

### Closient `v*` tags

Closient deploys testing on every push to `main` and production when a
`v*` tag's GitHub release is published. Record both environments against
one project:

```toml
[[project]]
id = "closient"
name = "Closient"
components = ["web", "api"]
```

```yaml
# .github/workflows/deploy-production.yml, after the deploy job succeeds
  record-release:
    needs: deploy
    if: startsWith(github.ref_name, 'v')
    runs-on: ubuntu-latest
    env:
      TOCSIN_URL: ${{ vars.TOCSIN_URL }}
      TOCSIN_TOKEN: ${{ secrets.TOCSIN_RELEASE_TOKEN }}
      VERSION: ${{ github.ref_name }}          # v1.42.0
      GH_TOKEN: ${{ github.token }}
    steps:
      - run: gh release view "$VERSION" -R "$GITHUB_REPOSITORY" --json body -q .body > notes.md
      - run: |
          tocsin release publish --project closient --version "$VERSION" \
            --notes-file notes.md \
            --release-url "https://github.com/$GITHUB_REPOSITORY/releases/tag/$VERSION" \
            --deployed-at now
```

```yaml
# .github/workflows/deploy-testing.yml, after each main deploy
      - run: |
          tocsin release deployed --project closient \
            --version "main-${GITHUB_SHA::12}" --environment testing
```

The testing deploys are stubs (no notes), so they are on the timeline next
to outages without filling the public release log.

## Hand-off for the status page

The status page's component history (TOC-47) reads deploys through the same
function as the API and the admin page, so the window bound, the limit and
the visibility rule cannot drift:

```rust
crate::http::timeline::read(&state, Viewer::Public, Some(component_id), since, limit).await
```
