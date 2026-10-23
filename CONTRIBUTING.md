# Contributing to tocsin

Thank you for helping. tocsin is licensed under the [GNU Affero General Public License, version 3 only](LICENSE), and every contribution is accepted under that same license.

## Sign off every commit (DCO)

tocsin uses the [Developer Certificate of Origin](https://developercertificate.org/) instead of a contributor license agreement. Each commit must carry a `Signed-off-by` line with your real name and email, certifying that you wrote the change or otherwise have the right to submit it under the AGPL-3.0:

```
Signed-off-by: Jane Doe <jane@example.com>
```

`git commit -s` adds it for you. CI checks every commit in a pull request and fails if one is not signed off; pull requests with unsigned commits are not merged.

## What that means for the license

There is no CLA, so nobody, the maintainers included, is granted extra rights over your contribution. Once outside contributions are merged, tocsin can only be relicensed with the agreement of every contributor, which makes AGPL-3.0 effectively permanent. That is intentional: tocsin stays open source, and nobody can take it closed.

## Before you open a pull request

The checks CI runs are listed in [AGENTS.md](AGENTS.md); run them locally first. Changes to the design go through [DESIGN.md](DESIGN.md) in the same pull request.
