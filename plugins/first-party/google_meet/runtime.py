"""Where the plugin keeps things, and whether it can join a meeting.

The browser runtime is a virtual environment in the plugin's data
directory holding Playwright, its Chromium and ``websockets``
(``prometheus plugins run google_meet install``); the saved Google
sign-in state is ``auth.json`` beside it (``... auth``). Meeting artifacts
go to ``<profile_home>/workspace/meetings/<meeting_id>/``.
"""

from __future__ import annotations

import glob
import importlib
import os
import platform
import sys
from pathlib import Path

# Set by ``register``.
CTX = None

# What ``install`` puts in the runtime.
PLAYWRIGHT = "playwright==1.55.0"
WEBSOCKETS = "websockets==15.0.1"


def supported() -> bool:
    return platform.system() in {"Linux", "Darwin"}


class Paths:
    def __init__(self, data_dir: str, profile_home: str) -> None:
        self.data = Path(data_dir)
        self.runtime = self.data / "runtime"
        bin_dir = "Scripts" if os.name == "nt" else "bin"
        self.python = self.runtime / bin_dir / ("python.exe" if os.name == "nt" else "python")
        self.browsers = self.runtime / "browsers"
        self.ready = self.runtime / ".ready"
        self.auth = self.data / "auth.json"
        self.nodes = self.data / "nodes.json"
        self.node_token = self.data / "node_token.json"
        self.meetings = Path(profile_home) / "workspace" / "meetings"
        self.active = self.meetings / ".active.json"


def paths() -> Paths:
    return Paths(CTX.data_dir, CTX.profile_home)


def runtime_ready(p: Paths) -> bool:
    return p.ready.is_file() and p.python.exists()


def available() -> bool:
    """The tools' ``check_fn``: a supported OS, the browser runtime and the
    saved Google sign-in state."""
    p = paths()
    return supported() and runtime_ready(p) and p.auth.is_file()


def config(key: str, default):
    value = CTX.get_config(key, default) if CTX is not None else default
    return default if value is None else value


def import_from_runtime(name: str):
    """Imports ``name`` from the browser runtime's packages (``websockets``
    for remote nodes lives there, not in the sidecar's environment)."""
    p = paths()
    for site in glob.glob(str(p.runtime / "lib" / "python*" / "site-packages")) + [
            str(p.runtime / "Lib" / "site-packages")]:
        if site not in sys.path and os.path.isdir(site):
            sys.path.append(site)
    try:
        return importlib.import_module(name)
    except ImportError as exc:
        raise RuntimeError(
            f"{name} is not installed: run `prometheus plugins run google_meet install`") from exc
