"""Resolve PROMETHEUS_HOME for standalone skill scripts.

Skill scripts may run outside the Prometheus process (e.g. system Python,
nix env, CI) where ``prometheus_constants`` is not importable.  This module
provides the same ``get_prometheus_home()`` and ``display_prometheus_home()``
contracts as ``prometheus_constants`` without requiring it on ``sys.path``.

When ``prometheus_constants`` IS available it is used directly so that any
future enhancements (profile resolution, Docker detection, etc.) are
picked up automatically.  The fallback path replicates the core logic
from ``prometheus_constants.py`` using only the stdlib.

All scripts under ``google-workspace/scripts/`` should import from here
instead of duplicating the ``PROMETHEUS_HOME = Path(os.getenv(...))`` pattern.
"""

from __future__ import annotations

import os
from pathlib import Path

try:
    from prometheus_constants import display_prometheus_home
    from prometheus_constants import get_prometheus_home
except (ModuleNotFoundError, ImportError):

    def get_prometheus_home() -> Path:
        """Return the Prometheus home directory (default: ~/.prometheus).

        Mirrors ``prometheus_constants.get_prometheus_home()``."""
        val = os.environ.get("PROMETHEUS_HOME", "").strip()
        return Path(val) if val else Path.home() / ".prometheus"

    def display_prometheus_home() -> str:
        """Return a user-friendly ``~/``-shortened display string.

        Mirrors ``prometheus_constants.display_prometheus_home()``."""
        home = get_prometheus_home()
        try:
            return "~/" + home.relative_to(Path.home()).as_posix()
        except ValueError:
            return str(home)
