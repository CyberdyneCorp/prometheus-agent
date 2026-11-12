#!/bin/sh
# Starts the virtual display of the prometheus-sandbox desktop variant and
# keeps the container alive for the agent's commands.
set -eu
Xvfb "$DISPLAY" -screen 0 "${SCREEN:-1280x800x24}" -nolisten tcp &
openbox &
exec sleep infinity
