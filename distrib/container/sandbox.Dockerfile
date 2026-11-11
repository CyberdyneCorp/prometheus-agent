# syntax=docker/dockerfile:1.7
# The `prometheus-sandbox` image: the default `terminal.container.image`
# of the container terminal backend (spec: execution-environments,
# Container backend options; distribution-and-update, Container image run
# modes), published per release for linux/amd64 and linux/arm64 and pinned
# by digest in the release manifest. Targets:
#
#   sandbox  `<version>`: a shell toolbox for agent commands
#   desktop  `<version>-desktop`: plus a virtual X display (Xvfb on :99)
#            for the bot screen

ARG BASE=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

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
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends xvfb xauth x11-utils xdotool scrot openbox \
 && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 desktop-start.sh /usr/local/bin/desktop-start
USER agent
ENV DISPLAY=:99
CMD ["desktop-start"]
