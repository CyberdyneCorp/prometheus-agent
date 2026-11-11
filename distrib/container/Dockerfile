# syntax=docker/dockerfile:1.7
# The Prometheus OCI image (spec: distribution-and-update, Container image
# run modes; Container filesystem, init and health). Built per release by
# the Release channels workflow from the context `cargo xtask release
# channels` renders: this directory plus bin/<TARGETARCH>/prometheus, the
# verified binary of the signed release archive. Targets:
#
#   runtime  `<version>`: binary, CA certificates, PulseAudio client tools
#   full     `<version>-full`: plus verified Node, CPython, uv and Chromium,
#            copied into /data/runtimes on first start
#
# Every image file is owned by root and not writable by the runtime user;
# the container works with --read-only plus a writable /data and a tmpfs
# /tmp. prometheus runs as PID 1 and acts as init: it reaps orphans and
# forwards SIGTERM/SIGINT to the daemon.

ARG BASE=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

FROM ${BASE} AS runtime
ARG TARGETARCH
ARG CHANNEL=stable
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates pulseaudio-utils alsa-utils libasound2-plugins \
 && rm -rf /var/lib/apt/lists/*
# ALSA's default device goes to the PulseAudio socket the compose `audio`
# profile mounts at /run/pulse/native.
COPY asound.conf /etc/asound.conf
COPY --chmod=0755 bin/${TARGETARCH}/prometheus /usr/local/bin/prometheus
RUN ln -s prometheus /usr/local/bin/pmt \
 && /usr/local/bin/prometheus install-receipt --owner image --channel "${CHANNEL}" \
 && groupadd --gid 10001 prometheus \
 && useradd --uid 10001 --gid 10001 --home-dir /data --no-create-home --shell /usr/sbin/nologin prometheus \
 && install -d -o 10001 -g 10001 -m 0700 /data
ENV PROMETHEUS_HOME=/data
VOLUME ["/data"]
USER prometheus
WORKDIR /data
# `status --liveness` asks the daemon's runtime watchdog; no port is
# published and the dashboard stays on its loopback default.
HEALTHCHECK --interval=30s --timeout=5s --retries=3 --start-period=20s \
  CMD ["prometheus", "status", "--liveness"]
# One daemon serves every profile under /data/profiles (multiplexed), so
# one published port per listener serves them all.
CMD ["prometheus", "--set", "gateway.multiplex=true", "daemon", "run"]

FROM runtime AS full
# Installed (download, signature and pinned-digest checks) at build time
# into a seed store; the daemon copies it into /data/runtimes on first start.
USER root
# Chromium's shared libraries (the managed Chromium is not a distro build).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libnss3 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 libxkbcommon0 \
      libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2t64 \
      libpango-1.0-0 libcairo2 fonts-liberation \
 && rm -rf /var/lib/apt/lists/*
RUN PROMETHEUS_HOME=/opt/prometheus/seed prometheus runtimes install node cpython uv chromium \
 && rm -rf /opt/prometheus/seed/profiles /opt/prometheus/seed/logs /opt/prometheus/seed/run \
 && chmod -R a+rX,go-w /opt/prometheus
USER prometheus
