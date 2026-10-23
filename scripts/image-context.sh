#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Unpacks the Linux release archives in DISTRIB into a container build
# context: CONTEXT/amd64/tocsin and CONTEXT/arm64/tocsin, the layout the
# Dockerfile copies from. Each archive is checked against its .sha256 first.
# Used by the release (.github/workflows/publish-image.yml) and by CI, which
# builds the image from one architecture's archive.
#
#   scripts/image-context.sh DISTRIB CONTEXT
set -eu

distrib=$1
context=$2
found=0
for pair in amd64:x86_64 arm64:aarch64; do
    arch=${pair%%:*}
    name=tocsin-${pair#*:}-unknown-linux-musl
    [ -f "$distrib/$name.tar.xz" ] || continue
    (cd "$distrib" && sha256sum --check --strict "$name.tar.xz.sha256")
    mkdir -p "$context/$arch"
    tar -xJf "$distrib/$name.tar.xz" -C "$context/$arch" --strip-components=1 "$name/tocsin"
    found=$((found + 1))
done
if [ "$found" -eq 0 ]; then
    echo "no tocsin-*-unknown-linux-musl.tar.xz archives in $distrib" >&2
    exit 1
fi
