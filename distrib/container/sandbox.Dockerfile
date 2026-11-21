# syntax=docker/dockerfile:1.7
# The `prometheus-sandbox` image: the default `terminal.container.image`
# of the container terminal backend (spec: execution-environments,
# Container backend options; distribution-and-update, Container image run
# modes), published per release for linux/amd64 and linux/arm64 and pinned
# by digest in the release manifest. Targets:
#
#   sandbox  `<version>`: a shell toolbox for agent commands
#   desktop  `<version>-desktop`: plus TigerVNC's Xvnc and a minimal Xfce
#            session: the bot screen's container when it is placed in the
#            container backend (Prometheus runs Xvnc and the session in it;
#            `desktop-start` runs them stand-alone on :99) and the pinned
#            `cua-driver` the computer_use tool drives that screen with
#            (spec: computer-use, Which machine is driven)

ARG BASE=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
# The computer-use driver: the version and digests of the binary's runtime
# index (crates/pmt-update/runtimes/index.json, `cua-driver`).
ARG CUA_DRIVER_VERSION=0.21.0
ARG CUA_DRIVER_SHA256_AMD64=cf29bfaad16737e31e72f38dad2a6424f24b21a4ac2edea885e83cb9d5034135
ARG CUA_DRIVER_SHA256_ARM64=6b3a308c6741dd8291aad22a308bd2ec8eb52eab64687be007ac86ceaf7e3307

FROM ${BASE} AS sandbox
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash ca-certificates coreutils curl file findutils git grep jq less \
      procps python3 python3-venv ripgrep sed tar unzip xz-utils zip \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 10001 agent \
 && useradd --uid 10001 --gid 10001 --create-home --shell /bin/bash agent \
 && install -d -o 10001 -g 10001 /workspace
USER agent
WORKDIR /workspace
CMD ["sleep", "infinity"]

FROM sandbox AS desktop
ARG CUA_DRIVER_VERSION
ARG CUA_DRIVER_SHA256_AMD64
ARG CUA_DRIVER_SHA256_ARM64
ARG TARGETARCH
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      tigervnc-standalone-server xfce4-session xfwm4 xfce4-panel xfdesktop4 dbus-x11 \
      xauth x11-utils xdotool scrot at-spi2-core \
 && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH:-amd64}" in \
      amd64) arch=linux-x86_64; sha="${CUA_DRIVER_SHA256_AMD64}" ;; \
      arm64) arch=linux-arm64; sha="${CUA_DRIVER_SHA256_ARM64}" ;; \
      *) echo "no cua-driver build for ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && url="https://github.com/trycua/cua/releases/download/cua-driver-rs-v${CUA_DRIVER_VERSION}/cua-driver-rs-${CUA_DRIVER_VERSION}-${arch}-binary.tar.gz" \
 && curl -fsSL -o /tmp/cua-driver.tar.gz "$url" \
 && echo "${sha}  /tmp/cua-driver.tar.gz" | sha256sum -c - \
 && install -d /opt/cua-driver \
 && tar -xzf /tmp/cua-driver.tar.gz -C /opt/cua-driver \
 && ln -s /opt/cua-driver/cua-driver /usr/local/bin/cua-driver \
 && rm /tmp/cua-driver.tar.gz
COPY --chmod=0755 desktop-start.sh /usr/local/bin/desktop-start
USER agent
ENV DISPLAY=:99
CMD ["desktop-start"]
