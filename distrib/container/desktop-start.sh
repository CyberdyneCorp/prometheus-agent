#!/bin/sh
# Starts the virtual display of the prometheus-sandbox desktop variant
# stand-alone (Xvnc on $DISPLAY, no TCP, RFB only on a Unix socket in /tmp)
# with its Xfce session, and keeps the container alive for the agent's
# commands. Prometheus's bot screen does not use this: it runs Xvnc and the
# session itself in a container started with `sleep infinity`.
set -eu
Xvnc "$DISPLAY" -geometry "${SCREEN:-1440x900}" -depth 24 -SecurityTypes None \
  -nolisten tcp -rfbport -1 -rfbunixpath /tmp/rfb.sock -rfbunixmode 0600 &
DISPLAY="$DISPLAY" dbus-launch --exit-with-session xfce4-session &
exec sleep infinity
