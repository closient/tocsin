# Static assets

Compiled into the binary by `src/web/assets.rs` and served at
`/static/<content hash>/<name>` with a year-long immutable cache. Changing a
file changes its URL, so nothing here ever needs a cache bust by hand.

| File | What | Licence |
|------|------|---------|
| `tocsin.css` | The stylesheet every page shares | AGPL-3.0-only |
| `htmx.min.js` | [htmx](https://htmx.org) 2.0.11, `dist/htmx.min.js` from the npm package, unmodified | 0BSD |
| `htmx-ext-sse.min.js` | [htmx-ext-sse](https://htmx.org/extensions/sse/) 2.2.4, `dist/sse.min.js` from the npm package, unmodified; the public status page's live updates | 0BSD, Copyright (c) 2023 Alexander Petros |

`htmx.min.js` sha256: `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`
(npm integrity `sha512-Thx/WtpeOQqSrqBCw/A1cwGJGg4UrVa3+sW0GmrM3p4gJgO89ecH4qtbnyzDDWFvBTqjnIMCgELTNt636dtamA==`).

`htmx-ext-sse.min.js` sha256: `98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136`
(npm integrity `sha512-LJmxVhykyflBWgh5PvbRidcyuqMHlgfajmmzumvKctv9puvsufeH6OaejSMZTNTnEI8O2wXXn4ZZtBdpEMqmEQ==`).

To upgrade htmx or its SSE extension, download the new `htmx.org` or
`htmx-ext-sse` tarball from the npm registry, check its integrity against
`npm view <package>@<version> dist.integrity`, copy the minified file from
`dist/` here and update this table and hash. A test pins each hash, so an
accidental edit fails CI. Both minified files ship without a licence comment;
the licences are 0BSD, which asks for no notice, and this table records them.
