# Recording releases from CI

tocsin's release log puts each release and deploy on the same timeline as
outages, so "what changed right before this broke" is one glance.
[releases.md](releases.md) describes the model, the API and the CLI; this
page is the recipe for wiring a pipeline to it.

## On the server

Declare the project and a token with the `release` scope:

```toml
[[project]]
id = "api"
name = "API"
url = "https://github.com/acme/api"

[[token]]
id = "api-release-ci"                   # recorded as created_by
secret = "${ENV:TOCSIN_RELEASE_TOKEN}"  # such as `openssl rand -hex 32`
scopes = ["release"]
```

Restart tocsin with `TOCSIN_RELEASE_TOKEN` in its environment, and store
the same value as a CI secret.

## In the pipeline

Two calls, both idempotent, in either order:

1. **Publish** when the release is cut: version, title, notes and a link.
2. **Deployed** when it reaches an environment.

### With the tocsin binary

The static musl binary runs on any Linux runner. Download it, check it, and
call it:

```yaml
on:
  release:
    types: [published]

jobs:
  record:
    runs-on: ubuntu-24.04
    env:
      TOCSIN_URL: https://status.example.com
      TOCSIN_TOKEN: ${{ secrets.TOCSIN_RELEASE_TOKEN }}
      VERSION: ${{ github.event.release.tag_name }}
    steps:
      - name: Install tocsin
        env:
          TOCSIN_VERSION: v0.3.0     # pin the client like any other tool
        run: |
          name=tocsin-x86_64-unknown-linux-musl
          base=https://github.com/closient/tocsin/releases/download/$TOCSIN_VERSION
          cd "$RUNNER_TEMP"
          curl -fsSLO "$base/$name.tar.xz" -fsSLO "$base/$name.tar.xz.sha256"
          sha256sum --check --strict "$name.tar.xz.sha256"
          tar -xJf "$name.tar.xz" --strip-components=1 "$name/tocsin"
          echo "$RUNNER_TEMP" >> "$GITHUB_PATH"

      - name: Record the release
        env:
          NOTES: ${{ github.event.release.body }}
          LINK: ${{ github.event.release.html_url }}
        run: |
          printf '%s' "$NOTES" |
          tocsin release publish --project api --version "$VERSION" --notes-file - \
            --release-url "$LINK"

      - name: Deploy
        run: ./deploy.sh "$VERSION"

      - name: Record the deploy
        run: tocsin release deployed --project api --version "$VERSION"
```

On an arm64 runner use `tocsin-aarch64-unknown-linux-musl`. The client
exits 2 for anything the server refuses (a 4xx: wrong token, unknown
project, a version that is not URL-safe) and 3 when the server cannot be
reached or answers 5xx, so a failed record fails the job. Re-running it is
safe.

### Without it

`curl` and `jq` are enough; [releases.md](releases.md#github-actions) has
the same pipeline in that form.

### Other environments

Pass `--environment staging` to both calls for a deploy that is not
production. Each project, version and environment is one row: the same
version deployed to staging and then production is two rows on the
timeline.

## tocsin records its own releases

tocsin's own release pipeline uses this recipe.
[`.github/workflows/record-release.yml`](../.github/workflows/record-release.yml)
runs after each GitHub Release is announced. It downloads that release's
own x86_64 musl binary, checks it against the published SHA-256, and
publishes the release, with the GitHub release notes and a link to the
release page, to the server in the repository variable `TOCSIN_URL` using
the secret `TOCSIN_RELEASE_TOKEN`. The server's side is
[`examples/config/releases.toml`](../examples/config/releases.toml).

So a release whose binary cannot publish a release fails its own pipeline.
If either setting is missing the job fails and says which: an unrecorded
release is a failure, not a skip.
