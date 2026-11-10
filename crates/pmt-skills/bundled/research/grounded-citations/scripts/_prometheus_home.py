"""Resolve PROMETHEUS_HOME for standalone skill scripts.

Skill scripts may run outside the Prometheus process (system Python, nix env,
CI) where ``prometheus_constants`` is not importable.  This module provides the
same ``get_prometheus_home()`` contract without requiring it on ``sys.path``.

When ``prometheus_constants`` IS available it is used directly so profile
resolution and any future enhancements are picked up automatically.
"""

from __future__ import annotations

import os
from pathlib import Path

try:
    from prometheus_constants import get_prometheus_home
except (ModuleNotFoundError, ImportError):

    def get_prometheus_home() -> Path:
        """Return the Prometheus home directory (default: ``~/.prometheus``)."""
        val = os.environ.get("PROMETHEUS_HOME", "").strip()
        return Path(val) if val else Path.home() / ".prometheus"
