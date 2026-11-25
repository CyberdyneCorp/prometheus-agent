"""disk-cleanup: tracks the ephemeral files the agent creates and removes
them when the session ends (spec: plugin-system, First-party disk-cleanup
plugin).

``pre_tool_call`` notes which paths a ``write_file``, ``patch`` or
``terminal`` call names that do not exist yet; ``post_tool_call`` tracks
those the call created (``test_*``, ``tmp_*``, ``*.test.*`` under the profile
home or ``<tmp>/prometheus-*``); ``on_session_end`` runs the automatic rules.
``/disk-cleanup`` shows and drives it.
"""

from __future__ import annotations

import os
import shlex
import threading
import time
from pathlib import Path
from typing import Any, Callable, Optional

from . import disk_cleanup as dc

_ctx = None
_lock = threading.Lock()

# (session, call id) -> (when noted, the argument paths absent before the call).
_absent: dict[tuple[str, str], tuple[float, frozenset[str]]] = {}
# A call whose post hook never fires (blocked) must not leak its entry.
_ABSENT_TTL_S = 3600.0


def _cleaner() -> dc.Cleaner:
    return dc.Cleaner(_ctx.profile_home, _ctx.temp_root, _ctx.data_dir)


def _absolute(path: str) -> Optional[Path]:
    """``path`` against the session's working directory; ``~`` paths are
    skipped (the sidecar's home is not the user's)."""
    if path.startswith("~"):
        return None
    candidate = Path(path)
    if not candidate.is_absolute():
        cwd = _ctx.session_cwd() if _ctx is not None else ""
        if not cwd:
            return None
        candidate = Path(cwd) / candidate
    return candidate


def _path_argument(args: dict[str, Any]) -> set[str]:
    path = args.get("path")
    return {path} if isinstance(path, str) and path else set()


def _command_paths(args: dict[str, Any]) -> set[str]:
    """Path-looking tokens of a terminal command; the rules filter them."""
    command = args.get("command")
    if not isinstance(command, str) or not command:
        return set()
    try:
        tokens = shlex.split(command, posix=os.name == "posix")
    except ValueError:
        return set()
    return {t for t in tokens if "/" in t or "\\" in t or t.startswith(("test_", "tmp_"))}


_EXTRACTORS: dict[str, Callable[[dict[str, Any]], set[str]]] = {
    "write_file": _path_argument,
    "patch": _path_argument,
    "terminal": _command_paths,
}


def _candidates(tool_name: str, args: Any) -> list[Path]:
    extractor = _EXTRACTORS.get(tool_name)
    if extractor is None or not isinstance(args, dict):
        return []
    return [p for p in (_absolute(s) for s in extractor(args)) if p is not None]


def _on_pre_tool_call(tool_name: str = "", args: Any = None, session_id: str = "",
                      tool_call_id: str = "", **_: Any) -> None:
    """Notes the argument paths that do not exist yet. Never raises: an
    error here would block the tool."""
    try:
        candidates = _candidates(tool_name, args)
        if not candidates or not tool_call_id:
            return
        absent = frozenset(str(p) for p in candidates if not p.exists())
    except Exception:  # noqa: BLE001 - an observer never blocks the call
        return
    now = time.time()
    with _lock:
        for key in [k for k, (noted, _a) in _absent.items() if now - noted > _ABSENT_TTL_S]:
            del _absent[key]
        _absent[(session_id, tool_call_id)] = (now, absent)


def _on_post_tool_call(tool_name: str = "", args: Any = None, session_id: str = "",
                       tool_call_id: str = "", **_: Any) -> None:
    """Tracks what this call created: a path its pre hook saw absent that
    exists now. Paths only seen in output carry no such proof."""
    with _lock:
        absent = _absent.pop((session_id, tool_call_id), (0.0, frozenset()))[1]
    if not absent:
        return
    cleaner = _cleaner()
    for path in _candidates(tool_name, args):
        if str(path) not in absent or not path.exists():
            continue
        category = cleaner.category_of(path)
        if category is not None:
            cleaner.track(str(path), category)


def _on_session_end(**_: Any) -> None:
    """Runs the automatic rules when anything is tracked."""
    cleaner = _cleaner()
    if not cleaner.load():
        return
    summary = cleaner.quick()
    if summary["deleted"] or summary["empty_dirs"]:
        cleaner.log(f"AUTO_QUICK (session end): deleted={summary['deleted']} "
                    f"dirs={summary['empty_dirs']} freed={dc.fmt_size(summary['freed'])}")


HELP = """\
/disk-cleanup - ephemeral file cleanup

  status                     Per-category totals and the 10 largest tracked files
  dry-run                    What quick would delete and what deep would list
  quick                      Run the automatic rules now
  deep                       Run quick, then list what needs your confirmation
  track <path> <category>    Track a path by hand
  forget <path>              Stop tracking a path (it is not deleted)

Categories: temp | test | research | download | browser-profile | cron-output | other
Only the profile home and <tmp>/prometheus-* are ever touched."""


def _summary(summary: dict[str, Any]) -> str:
    text = (f"[disk-cleanup] Cleaned {summary['deleted']} files + {summary['empty_dirs']} empty dirs, "
            f"freed {dc.fmt_size(summary['freed'])}.")
    if summary["errors"]:
        text += f"\n  {len(summary['errors'])} error(s); see cleanup.log."
    return text


def _items(header: str, items: list[dict], indent: str) -> list[str]:
    size = dc.fmt_size(sum(int(i.get("size", 0)) for i in items))
    return [header.format(n=len(items), size=size)] + [f"{indent}[{i['category']}] {i['path']}" for i in items]


def _dry_run(_argv: list[str]) -> str:
    auto, confirm = _cleaner().dry_run()
    lines = ["Dry run (nothing deleted):"]
    lines += _items("  Deleted automatically: {n} files ({size})", auto, "    ")
    lines += _items("  Needs confirmation:    {n} files ({size})", confirm, "    ")
    return "\n".join(lines)


def _deep(_argv: list[str]) -> str:
    cleaner = _cleaner()
    lines = [_summary(cleaner.quick())]
    _auto, confirm = cleaner.dry_run()
    if confirm:
        lines += _items("\n{n} item(s) need confirmation ({size}):", confirm, "  ")
        lines.append("\nDelete them yourself, or `/disk-cleanup forget <path>` to stop tracking one.")
    return "\n".join(lines)


def _track(argv: list[str]) -> str:
    if len(argv) < 3:
        return "Usage: /disk-cleanup track <path> <category>"
    path = _absolute(argv[1])
    if path is None:
        return f"Not tracked: {argv[1]} (give an absolute path)"
    tracked, why = _cleaner().track(str(path), argv[2])
    return f"Tracked {path} as '{argv[2]}'." if tracked else f"Not tracked: {path} ({why})"


def _forget(argv: list[str]) -> str:
    if len(argv) < 2:
        return "Usage: /disk-cleanup forget <path>"
    path = _absolute(argv[1]) or Path(argv[1])
    n = _cleaner().forget(str(path))
    return f"Stopped tracking {path}." if n else f"Not tracked: {path}"


_SUBCOMMANDS: dict[str, Callable[[list[str]], str]] = {
    "status": lambda _argv: dc.format_status(_cleaner().status()),
    "dry-run": _dry_run,
    "quick": lambda _argv: _summary(_cleaner().quick()),
    "deep": _deep,
    "track": _track,
    "forget": _forget,
}


def _slash(raw: str) -> str:
    argv = raw.strip().split()
    if not argv or argv[0] in {"help", "-h", "--help"}:
        return HELP
    handler = _SUBCOMMANDS.get(argv[0])
    if handler is None:
        return f"Unknown subcommand: {argv[0]}\n\n{HELP}"
    return handler(argv)


def register(ctx) -> None:
    global _ctx
    _ctx = ctx
    ctx.register_hook("pre_tool_call", _on_pre_tool_call)
    ctx.register_hook("post_tool_call", _on_post_tool_call)
    ctx.register_hook("on_session_end", _on_session_end)
    ctx.register_command("disk-cleanup", handler=_slash,
                         description="Track and clean up the agent's ephemeral files",
                         args_hint="status|dry-run|quick|deep|track <path> <category>|forget <path>")
