# Neolink Docker image build scripts
# Copyright (c) 2020 George Hilliard,
#                    Andrew King,
#                    Miroslav Šedivý
# SPDX-License-Identifier: AGPL-3.0-only

# Base images pinned by version and digest (multi-arch index); bump them together with
# the CI toolchain (.github/actions/setup-rust)
FROM docker.io/library/rust:1.91.0-slim-bookworm@sha256:81c97592d519ba7397e8adccb0345fb5a91965c0a44b82af83bd1ef06c36cef5 AS build
ARG TARGETPLATFORM

ENV DEBIAN_FRONTEND=noninteractive
WORKDIR /usr/local/src/neolink
COPY . /usr/local/src/neolink

# Build the main program or copy from artifact
#
# We prefer copying from artifact to reduce
# build time on the github runners
#
# Because of this though, during normal
# github runner ops we are not testing the
# docker to see if it will build from scratch
# so if it is failing please make a PR
#
# hadolint ignore=DL3008
RUN  echo "TARGETPLATFORM: ${TARGETPLATFORM}"; \
  if [ -f "${TARGETPLATFORM}/neolink" ]; then \
    echo "Restoring from artifact"; \
    mkdir -p /usr/local/src/neolink/target/release/; \
    cp "${TARGETPLATFORM}/neolink" "/usr/local/src/neolink/target/release/neolink"; \
  else \
    echo "Building from scratch"; \
    apt-get update && \
        apt-get upgrade -y && \
        apt-get install -y --no-install-recommends \
          build-essential \
          openssl \
          libssl-dev \
          ca-certificates \
          libgstrtspserver-1.0-dev \
          libgstreamer1.0-dev \
          libgtk2.0-dev \
          protobuf-compiler \
          libglib2.0-dev && \
        apt-get clean -y && rm -rf /var/lib/apt/lists/* ; \
    cargo build --release --locked; \
  fi

# Create the release container. Match the base OS used to build
FROM docker.io/library/debian:bookworm-20261005-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
ARG TARGETPLATFORM
ARG REPO
ARG VERSION
ARG OWNER

LABEL description="An image for the neolink program which is a reolink camera to rtsp translator"
LABEL repository="$REPO"
LABEL version="$VERSION"
LABEL maintainer="$OWNER"

# hadolint ignore=DL3008
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y --no-install-recommends \
        openssl \
        dnsutils \
        iputils-ping \
        ca-certificates \
        libgstrtspserver-1.0-0 \
        libgstreamer1.0-0 \
        gstreamer1.0-tools \
        gstreamer1.0-x \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-libav && \
    apt-get clean -y && rm -rf /var/lib/apt/lists/*

COPY --from=build \
  /usr/local/src/neolink/target/release/neolink \
  /usr/local/bin/neolink
# The entrypoint of earlier images, for setups that name it (`entrypoint: /entrypoint.sh`)
COPY docker/entrypoint.sh /entrypoint.sh

RUN gst-inspect-1.0; \
    chmod +x "/usr/local/bin/neolink" && \
    "/usr/local/bin/neolink" --version && \
    mkdir -m 0700 /root/.config/ /var/lib/neolink

# neolink is PID 1 (exec form), so `docker stop` reaches it as SIGTERM and it logs the
# cameras out before exiting (PLAN 1.9). NEO_LINK_MODE picks the subcommand. A command
# that is not neolink's (`sh -c "..."`, a script) is run in neolink's place, as the old
# /entrypoint.sh did.
# Mount /var/lib/neolink to keep what neolink learns about each camera (battery level,
# streams, last snapshot) across restarts.
ENV NEO_LINK_MODE="rtsp" NEO_LINK_PORT=8554 NEOLINK_STATE_DIR=/var/lib/neolink
VOLUME /var/lib/neolink
ENTRYPOINT ["/usr/local/bin/neolink"]
CMD ["--config", "/etc/neolink.toml"]
EXPOSE ${NEO_LINK_PORT}
