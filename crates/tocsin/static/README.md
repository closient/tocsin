# Static assets

Compiled into the binary by `src/web/assets.rs` and served at
`/static/<content hash>/<name>` with a year-long immutable cache. Changing a
file changes its URL, so nothing here ever needs a cache bust by hand.

| File | What | Licence |
|------|------|---------|
| `tocsin.css` | The stylesheet every page shares | AGPL-3.0-only |
| `htmx.min.js` | [htmx](https://htmx.org) 2.0.11, `dist/htmx.min.js` from the npm package, unmodified | 0BSD |

`htmx.min.js` sha256: `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`
(npm integrity `sha512-Thx/WtpeOQqSrqBCw/A1cwGJGg4UrVa3+sW0GmrM3p4gJgO89ecH4qtbnyzDDWFvBTqjnIMCgELTNt636dtamA==`).

To upgrade htmx, download the new `htmx.org` tarball from the npm registry,
check its integrity against `npm view htmx.org@<version> dist.integrity`, copy
`dist/htmx.min.js` here and update this table and hash. A test pins the hash,
so an accidental edit fails CI.
