"""The background bot process (spec: plugin-system, First-party google_meet
plugin): one active meeting at a time, recorded in
``<profile_home>/workspace/meetings/.active.json``. The bot is a detached
child reached only through files in its meeting directory
(``status.json``, ``transcript.txt``, ``say_queue.jsonl``,
``leave.request``), so a tool call never waits on it and any later process
of the profile can find it.
"""

from __future__ import annotations

import contextlib
import json
import os
import signal
import subprocess
import time
import uuid
from pathlib import Path
from typing import Any, Optional

from . import runtime
from .jsonfile import file_lock, read_json, write_json
from .meeturl import REFUSAL, is_meet_url, meeting_id

NO_ACTIVE = {"ok": False, "reason": "no active meeting"}
BOT_SCRIPT = Path(__file__).with_name("meet_bot.py")
LEAVE_REQUEST = "leave.request"


def _pid_alive(pid: int) -> bool:
    if not pid:
        return False
    # Reap our own exited child, so a zombie does not count as alive.
    with contextlib.suppress(ChildProcessError, OSError):
        if os.waitpid(pid, os.WNOHANG)[0] == pid:
            return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # Started from another sandboxed process: it exists.
        return True
    except OSError:
        return False
    return True


def _alive(active: dict[str, Any]) -> bool:
    bot = read_json(Path(active.get("out_dir", "")) / "status.json") or {}
    return _pid_alive(int(active.get("pid", 0))) and not bot.get("exited")


def _read_active(p: runtime.Paths) -> Optional[dict[str, Any]]:
    data = read_json(p.active)
    return data if isinstance(data, dict) else None


def start(url: str, *, guest_name: str = "Prometheus Agent", duration: Optional[str] = None,
          headed: bool = False, mode: str = "transcribe", session_id: Optional[str] = None) -> dict[str, Any]:
    """Starts the bot for ``url`` in the background (stopping a running one)
    and returns at once."""
    if not is_meet_url(url):
        return {"ok": False, "error": REFUSAL}
    p = runtime.paths()
    if not runtime.runtime_ready(p):
        return {"ok": False, "error": "the browser runtime is not installed: run "
                                      "`prometheus plugins run google_meet install`"}
    active = _read_active(p)
    if active and _alive(active):
        stop(reason="replaced by a new meet_join")
    mid = meeting_id(url)
    out = p.meetings / mid
    out.mkdir(parents=True, exist_ok=True)
    for name in ("transcript.txt", "status.json", LEAVE_REQUEST, "say_queue.jsonl"):
        with contextlib.suppress(OSError):
            (out / name).unlink()
    env = {**os.environ, "PROMETHEUS_MEET_URL": url, "PROMETHEUS_MEET_OUT_DIR": str(out),
           "PROMETHEUS_MEET_GUEST_NAME": guest_name, "PROMETHEUS_MEET_MODE": mode,
           "PLAYWRIGHT_BROWSERS_PATH": str(p.browsers)}
    for value, var in ((headed and "1", "PROMETHEUS_MEET_HEADED"),
                       (str(p.auth) if p.auth.is_file() else "", "PROMETHEUS_MEET_AUTH_STATE"),
                       (duration, "PROMETHEUS_MEET_DURATION"),
                       (runtime.config("realtime_model", ""), "PROMETHEUS_MEET_REALTIME_MODEL"),
                       (runtime.config("realtime_voice", ""), "PROMETHEUS_MEET_REALTIME_VOICE"),
                       (runtime.config("realtime_instructions", ""), "PROMETHEUS_MEET_REALTIME_INSTRUCTIONS")):
        if value:
            env[var] = str(value)
    log_path = out / "bot.log"
    with open(log_path, "ab", buffering=0) as log:
        # Detached: it outlives this call and is stopped through its files.
        proc = subprocess.Popen([str(p.python), str(BOT_SCRIPT)], stdin=subprocess.DEVNULL,
                                stdout=log, stderr=subprocess.STDOUT, env=env, cwd=str(out),
                                start_new_session=True, close_fds=True)
    record = {"pid": proc.pid, "meeting_id": mid, "out_dir": str(out), "url": url,
              "started_at": time.time(), "session_id": session_id, "log_path": str(log_path), "mode": mode}
    write_json(p.active, record)
    return {"ok": True, **record}


def status() -> dict[str, Any]:
    """The active meeting's state, from its record and the bot's status file."""
    active = _read_active(runtime.paths())
    if not active:
        return dict(NO_ACTIVE)
    bot = read_json(Path(active.get("out_dir", "")) / "status.json") or {}
    return {"ok": True, "alive": _alive(active), "pid": active.get("pid"),
            "meetingId": active.get("meeting_id"), "url": active.get("url"),
            "startedAt": active.get("started_at"), "outDir": active.get("out_dir"),
            "sessionId": active.get("session_id"), **bot}


def transcript(last: Optional[int] = None) -> dict[str, Any]:
    """The captions so far (the last ``last`` lines when given)."""
    active = _read_active(runtime.paths())
    if not active:
        return dict(NO_ACTIVE)
    path = Path(active.get("out_dir", "")) / "transcript.txt"
    try:
        lines = [ln for ln in path.read_text(encoding="utf-8", errors="replace").splitlines() if ln.strip()]
    except OSError:
        lines = []
    return {"ok": True, "meetingId": active.get("meeting_id"),
            "lines": lines[-last:] if last else lines, "total": len(lines), "path": str(path)}


def enqueue_say(text: str) -> dict[str, Any]:
    """Queues ``text`` for the bot to speak (realtime meetings only)."""
    text = (text or "").strip()
    if not text:
        return {"ok": False, "reason": "text is required"}
    active = _read_active(runtime.paths())
    if not active:
        return dict(NO_ACTIVE)
    if active.get("mode") != "realtime":
        return {"ok": False, "reason": "the active meeting is in transcribe mode: join with "
                                       "mode='realtime' to speak"}
    out = Path(active.get("out_dir", ""))
    if not out.is_dir():
        return {"ok": False, "reason": f"meeting directory missing: {out}"}
    queue = out / "say_queue.jsonl"
    entry = {"id": uuid.uuid4().hex[:12], "text": text}
    with file_lock(queue), queue.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")
    return {"ok": True, "meetingId": active.get("meeting_id"), "enqueued_id": entry["id"]}


def stop(*, reason: str = "requested") -> dict[str, Any]:
    """Asks the bot to leave (a ``leave.request`` file, then SIGTERM), waits
    up to 10 s, kills it if it is still there and clears the record."""
    p = runtime.paths()
    active = _read_active(p)
    if not active:
        return dict(NO_ACTIVE)
    pid = int(active.get("pid", 0))
    out = Path(active.get("out_dir", ""))
    with contextlib.suppress(OSError):
        (out / LEAVE_REQUEST).write_text(reason, encoding="utf-8")
    if _alive(active):
        with contextlib.suppress(OSError):
            os.kill(pid, signal.SIGTERM)
        for _ in range(40):
            if not _alive(active):
                break
            time.sleep(0.25)
        else:
            with contextlib.suppress(OSError):
                os.kill(pid, signal.SIGKILL)
    with contextlib.suppress(OSError):
        p.active.unlink()
    return {"ok": True, "reason": reason, "meetingId": active.get("meeting_id"),
            "transcriptPath": str(out / "transcript.txt")}
