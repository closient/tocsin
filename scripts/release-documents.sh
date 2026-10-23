#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Builds the documents a release ships beside the binaries: the config JSON
# Schema, the OpenAPI document, and a sha256 file covering both (dist's
# sha256.sum covers only the archives). `dist build` runs this from the
# workspace root (dist-workspace.toml, `extra-artifacts`).
#
# The documents are regenerated from the tagged code. If the committed copies
# differ, the release fails rather than ship a document that disagrees with
# the binary.
set -eu

cargo run --locked --quiet --package tocsin -- config schema > schema/tocsin.schema.json
cargo run --locked --quiet --package tocsin -- openapi > schema/openapi.json
git diff --exit-code -- schema/

mkdir -p target
(cd schema && sha256sum tocsin.schema.json openapi.json) > target/documents.sha256
