# SPDX-License-Identifier: AGPL-3.0-only
#
# The tocsin image: the release's static musl binary on distroless, running
# as the non-root `nonroot` user (65532). The build context holds one binary
# per architecture, `amd64/tocsin` and `arm64/tocsin`, unpacked from the
# release archives (.github/workflows/publish-image.yml), so the image holds
# exactly what the release does. docs/install.md#container-image.

# The volumes exist in the image owned by nonroot, so a named volume mounted
# there starts out writable. Runs on the build machine: it only makes folders.
FROM --platform=$BUILDPLATFORM busybox:1.37.0-musl@sha256:5cec3fc171c87218698e85a52af7087de727372aae264a787b8112901a5b0092 AS folders
RUN mkdir -p /out/config /out/data && chown -R 65532:65532 /out

FROM gcr.io/distroless/static-debian13:nonroot@sha256:d62067879de890a1def9da0e7766b7d20810a5e5555cef8506b752437d7a3e88
ARG TARGETARCH
ARG VERSION=dev
LABEL org.opencontainers.image.title="tocsin" \
      org.opencontainers.image.description="Single-binary uptime monitor, status page and release log." \
      org.opencontainers.image.source="https://github.com/closient/tocsin" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=folders /out/ /
COPY --chmod=0755 ${TARGETARCH}/tocsin /usr/local/bin/tocsin
USER 65532:65532
VOLUME ["/config", "/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tocsin"]
CMD ["serve", "--config", "/config", "--data", "/data"]
