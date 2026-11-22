"""Small JSON file helpers shared by the sidecar and the bot process (a
leaf module: no sibling imports)."""

from __future__ import annotations

import contextlib
import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any, Iterator, Optional


def read_json(path: Path) -> Optional[Any]:
    """The parsed file, or None when missing, unreadable or malformed."""
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def write_json(path: Path, data: Any, mode: int = 0o600) -> None:
    """Writes ``data`` atomically (a temporary file renamed into place)."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, staging = tempfile.mkstemp(prefix=f".{path.name}.", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2)
        os.chmod(staging, mode)
        os.replace(staging, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(staging)
        raise


@contextlib.contextmanager
def file_lock(path: Path, timeout: float = 10.0) -> Iterator[None]:
    """A cross-process lock on ``<path>.lock`` (the bot rewrites the say
    queue; an append landing in between would be lost)."""
    lock_path = Path(str(path) + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with open(lock_path, "a+") as handle:
        deadline = time.monotonic() + timeout
        while True:
            try:
                _lock(handle)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise TimeoutError(f"timed out waiting for {lock_path}") from None
                time.sleep(0.05)
        try:
            yield
        finally:
            _unlock(handle)


if os.name == "nt":  # pragma: no cover - the plugin runs on Linux and macOS
    import msvcrt

    def _lock(handle) -> None:
        msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)

    def _unlock(handle) -> None:
        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
else:
    import fcntl

    def _lock(handle) -> None:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

    def _unlock(handle) -> None:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
