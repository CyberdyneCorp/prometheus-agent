"""The cleanup rules behind the disk-cleanup plugin (spec: plugin-system,
First-party disk-cleanup plugin).

Scope: the profile home and ``<tmp>/prometheus-*`` only. Never a Windows
mount, a protected top-level state directory of the profile, a user
workspace tree or anything inside a git checkout.

Automatic (``quick``, also at session end): ``test`` files at any age,
``temp`` older than 7 days, ``cron-output`` older than 14 days, and empty
directories outside the protected trees. Listed for confirmation only
(``deep``): ``research`` older than 30 days beyond the 10 newest,
``browser-profile`` older than 14 days and any tracked file over 500 MB.
State: ``tracked.json`` and the append-only ``cleanup.log`` in the plugin's
data directory.
"""

from __future__ import annotations

import contextlib
import json
import re
import shutil
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Iterator, Optional

LARGE_FILE_BYTES = 500 * 1024 * 1024
RESEARCH_KEEP_NEWEST = 10

ALLOWED_CATEGORIES = frozenset({
    "temp", "test", "research", "download", "browser-profile", "cron-output", "other"})

# Top-level entries of the profile home that are never tracked, deleted or
# swept: state directories, and the user's own workspace trees.
PROTECTED_TOP_LEVEL = frozenset({
    # Named by the spec.
    "logs", "skills", "plugins", "cache", "trajectories", "snapshots", "kanban",
    # The rest of the profile layout and the state other subsystems keep.
    "blobs", "harness", "scripts", "python", "memories", "themes", "backups", "migrations",
    "plugin-data", "run", "evolve", "evals", "crash", "tui", "mcp", "hooks", "agents",
    "experience", "pets", "spill", "platforms", "hook_outputs",
    # User workspace trees.
    "workspace", "projects", "worktrees",
})

# Directories the empty-directory sweep never descends into.
PRUNE_DIRS = frozenset({".git", "node_modules", "venv", ".venv", "site-packages", "__pycache__"})

TEST_PREFIXES = ("test_",)
TEMP_PREFIXES = ("tmp_",)
TEST_INFIX = re.compile(r"^[^.]+\.test\.[^.]+$")
WINDOWS_MOUNT = re.compile(r"^/mnt/[a-zA-Z](/|$)")


def fmt_size(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} PB"


def _resolve(path: Path) -> Optional[Path]:
    try:
        return path.resolve()
    except (OSError, RuntimeError):
        return None


def _name_category(name: str) -> Optional[str]:
    """``test`` or ``temp`` for a name the plugin tracks, else None."""
    if name.startswith(TEST_PREFIXES) or TEST_INFIX.match(name):
        return "test"
    if name.startswith(TEMP_PREFIXES):
        return "temp"
    return None


class Cleaner:
    """The rules for one profile: its home, the host's temporary directory
    and the plugin's data directory."""

    def __init__(self, profile_home: str, temp_root: str, data_dir: str) -> None:
        self.home = _resolve(Path(profile_home)) or Path(profile_home)
        self.tmp = _resolve(Path(temp_root)) or Path(temp_root)
        self.data = Path(data_dir)

    # Scope.

    def _relative_to_home(self, resolved: Path) -> Optional[tuple[str, ...]]:
        with contextlib.suppress(ValueError):
            return resolved.relative_to(self.home).parts
        return None

    def _tmp_root_of(self, resolved: Path) -> Optional[Path]:
        """The ``<tmp>/prometheus-*`` directory holding ``resolved``, if any."""
        with contextlib.suppress(ValueError):
            parts = resolved.relative_to(self.tmp).parts
            if len(parts) >= 2 and parts[0].startswith("prometheus-"):
                return self.tmp / parts[0]
        return None

    def in_scope(self, path: Path) -> bool:
        """Under the profile home or ``<tmp>/prometheus-*``, never a Windows mount."""
        resolved = _resolve(path)
        if resolved is None or WINDOWS_MOUNT.match(resolved.as_posix()):
            return False
        return self._relative_to_home(resolved) is not None or self._tmp_root_of(resolved) is not None

    def is_protected(self, path: Path) -> bool:
        """The profile home itself, a protected top-level tree, or a git checkout."""
        resolved = _resolve(path)
        if resolved is None:
            return True
        rel = self._relative_to_home(resolved)
        if rel is not None:
            if not rel or rel[0] in PROTECTED_TOP_LEVEL:
                return True
            if rel[0] == "cron" and not (len(rel) >= 3 and rel[1] == "output"):
                return True
            root = self.home
        else:
            root = self._tmp_root_of(resolved)
            if root is None:
                return True
        return self._inside_git_checkout(resolved, root)

    @staticmethod
    def _inside_git_checkout(resolved: Path, root: Path) -> bool:
        """A ``.git`` entry between ``root`` and ``resolved``: a repository the
        user keeps there manages its own files."""
        for parent in resolved.parents:
            if (parent / ".git").exists():
                return True
            if parent == root:
                return False
        return False

    def category_of(self, path: Path) -> Optional[str]:
        """The category a newly created file is tracked under, or None."""
        name_category = _name_category(path.name)
        if name_category is None or not self.in_scope(path) or self.is_protected(path):
            return None
        rel = self._relative_to_home(_resolve(path) or path)
        if rel is not None and rel[0] == "cron":
            return "cron-output"
        return name_category

    # State.

    def _state(self, name: str) -> Path:
        return self.data / name

    def log(self, message: str) -> None:
        """Appends to ``cleanup.log``; never breaks the agent loop."""
        with contextlib.suppress(OSError):
            self.data.mkdir(parents=True, exist_ok=True)
            stamp = datetime.now(UTC).strftime("%Y-%m-%d %H:%M:%S")
            with open(self._state("cleanup.log"), "a", encoding="utf-8") as f:
                f.write(f"[{stamp}] {message}\n")

    def load(self) -> list[dict[str, Any]]:
        """``tracked.json``, restored from its backup when unreadable."""
        for name, note in (("tracked.json", None), ("tracked.json.bak", "restored from tracked.json.bak")):
            path = self._state(name)
            if not path.exists():
                continue
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if isinstance(data, list):
                if note:
                    self.log(f"WARN: tracked.json unreadable; {note}")
                return [i for i in data if isinstance(i, dict) and {"path", "category", "timestamp"} <= i.keys()]
        return []

    def save(self, tracked: list[dict[str, Any]]) -> None:
        """Atomic write, keeping the previous file as ``tracked.json.bak``."""
        self.data.mkdir(parents=True, exist_ok=True)
        target = self._state("tracked.json")
        staging = self._state("tracked.json.tmp")
        staging.write_text(json.dumps(tracked, indent=2), encoding="utf-8")
        if target.exists():
            shutil.copy2(target, self._state("tracked.json.bak"))
        staging.replace(target)

    # Tracking.

    def track(self, path_str: str, category: str) -> tuple[bool, str]:
        """Tracks a path; ``(tracked, why not)``."""
        if category not in ALLOWED_CATEGORIES:
            return False, f"unknown category '{category}'"
        path = _resolve(Path(path_str))
        if path is None or not path.exists():
            self.log(f"SKIP: {path_str} (does not exist)")
            return False, "it does not exist"
        if not self.in_scope(path):
            self.log(f"REJECT: {path} (outside the profile home and <tmp>/prometheus-*)")
            return False, "it is outside the profile home and <tmp>/prometheus-*"
        if self.is_protected(path):
            self.log(f"REJECT: {path} (protected tree)")
            return False, "it is in a protected directory or a user workspace tree"
        tracked = self.load()
        if any(item["path"] == str(path) for item in tracked):
            return False, "it is already tracked"
        size = path.stat().st_size if path.is_file() else 0
        tracked.append({"path": str(path), "timestamp": datetime.now(UTC).isoformat(),
                        "category": category, "size": size})
        self.save(tracked)
        self.log(f"TRACKED: {path} ({category}, {fmt_size(size)})")
        return True, ""

    def forget(self, path_str: str) -> int:
        """Stops tracking a path without deleting it; how many entries went."""
        path = _resolve(Path(path_str)) or Path(path_str)
        tracked = self.load()
        kept = [i for i in tracked if (_resolve(Path(i["path"])) or Path(i["path"])) != path]
        removed = len(tracked) - len(kept)
        if removed:
            self.save(kept)
            self.log(f"FORGOT: {path} ({removed} entries)")
        return removed

    # Rules.

    def _live(self, tracked: list[dict], *, log_stale: bool = False) -> Iterator[tuple[dict, Path, int]]:
        now = datetime.now(UTC)
        for item in tracked:
            path = Path(item["path"])
            if path.exists():
                with contextlib.suppress(ValueError):
                    yield item, path, (now - datetime.fromisoformat(item["timestamp"])).days
            elif log_stale:
                self.log(f"STALE: {path} (removed from tracking)")

    @staticmethod
    def _automatic(category: str, age: int) -> bool:
        return category == "test" or (category == "temp" and age > 7) or (category == "cron-output" and age > 14)

    def _confirmable(self, live: list[tuple[dict, Path, int]]) -> list[dict]:
        """What ``deep`` lists for confirmation."""
        research = sorted((i for i in live if i[0]["category"] == "research"),
                          key=lambda i: i[0]["timestamp"], reverse=True)
        old_research = {id(item) for item, _, age in research[RESEARCH_KEEP_NEWEST:] if age > 30}
        out = []
        for item, _path, age in live:
            if self._automatic(item["category"], age):
                continue
            if (id(item) in old_research or (item["category"] == "browser-profile" and age > 14)
                    or int(item.get("size", 0)) > LARGE_FILE_BYTES):
                out.append(item)
        return out

    def _eligible(self, path: Path) -> bool:
        """Still in scope and unprotected: a stale entry is never acted on."""
        return self.in_scope(path) and not self.is_protected(path)

    def dry_run(self) -> tuple[list[dict], list[dict]]:
        """``(deleted automatically, needs confirmation)``, touching nothing."""
        live = [entry for entry in self._live(self.load()) if self._eligible(entry[1])]
        auto = [item for item, _path, age in live if self._automatic(item["category"], age)]
        return auto, self._confirmable(live)

    def quick(self) -> dict[str, Any]:
        """The automatic rules; ``{deleted, empty_dirs, freed, errors}``."""
        deleted = freed = 0
        kept: list[dict] = []
        errors: list[str] = []
        for item, path, age in self._live(self.load(), log_stale=True):
            if not self._eligible(path):
                self.log(f"SKIP: {path} (protected or out of scope; dropped from tracking)")
                continue
            if not self._automatic(item["category"], age):
                kept.append(item)
                continue
            error = self._delete(item, path)
            if error is None:
                deleted += 1
                freed += int(item.get("size", 0))
            else:
                errors.append(error)
                kept.append(item)
        empty = self._sweep_empty_dirs()
        self.save(kept)
        self.log(f"QUICK_SUMMARY: {deleted} files, {empty} dirs, {fmt_size(freed)}")
        return {"deleted": deleted, "empty_dirs": empty, "freed": freed, "errors": errors}

    def _delete(self, item: dict, path: Path) -> Optional[str]:
        try:
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
            else:
                path.unlink()
        except OSError as e:
            self.log(f"ERROR deleting {path}: {e}")
            return f"{path}: {e}"
        self.log(f"DELETED: {path} ({item['category']}, {fmt_size(int(item.get('size', 0)))})")
        return None

    @staticmethod
    def _subdirs(directory: Path, exclude: frozenset) -> list[Path]:
        try:
            return [c for c in directory.iterdir()
                    if c.is_dir() and not c.is_symlink() and c.name not in exclude]
        except OSError:
            return []

    def _sweep_empty_dirs(self) -> int:
        """Removes empty directories in the profile home's unprotected trees,
        post-order so a parent emptied by its children goes too."""
        removed = 0
        tops = [d for d in self._subdirs(self.home, PROTECTED_TOP_LEVEL | PRUNE_DIRS | {"cron"})
                if not (d / ".git").exists()]
        stack: list[tuple[Path, bool]] = [(top, False) for top in tops]
        while stack:
            directory, visited = stack.pop()
            if visited:
                with contextlib.suppress(OSError):
                    if not any(directory.iterdir()):
                        directory.rmdir()
                        removed += 1
                        self.log(f"DELETED: {directory} (empty dir)")
                continue
            if (directory / ".git").exists():
                continue
            stack.append((directory, True))
            stack.extend((child, False) for child in self._subdirs(directory, PRUNE_DIRS))
        return removed

    def status(self) -> dict[str, Any]:
        """Per-category totals and the ten largest tracked files."""
        tracked = self.load()
        categories: dict[str, dict] = {}
        for item in tracked:
            c = categories.setdefault(item["category"], {"count": 0, "size": 0})
            c["count"] += 1
            c["size"] += int(item.get("size", 0))
        largest = sorted(((i["path"], int(i.get("size", 0)), i["category"]) for i in tracked
                          if Path(i["path"]).exists()), key=lambda x: x[1], reverse=True)
        return {"categories": categories, "top10": largest[:10], "total_tracked": len(tracked)}


def format_status(s: dict[str, Any]) -> str:
    lines = [f"{'Category':<20} {'Files':>6}  {'Size':>10}", "-" * 40]
    for category, d in sorted(s["categories"].items(), key=lambda x: x[1]["size"], reverse=True):
        lines.append(f"{category:<20} {d['count']:>6}  {fmt_size(d['size']):>10}")
    if not s["categories"]:
        lines.append("(nothing tracked yet)")
    lines += ["", "Top 10 largest tracked files:"]
    if not s["top10"]:
        lines.append("  (none)")
    for rank, (path, size, category) in enumerate(s["top10"], 1):
        lines.append(f"  {rank:>2}. {fmt_size(size):>8}  [{category}]  {path}")
    return "\n".join(lines)
