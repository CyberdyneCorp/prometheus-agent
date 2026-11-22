"""The ``google_meet`` toolset: meet_join, meet_status, meet_transcript,
meet_say and meet_leave. Each runs locally, or on a remote node with
``node=<name>`` (``auto``: the only approved node)."""

from __future__ import annotations

import json
from typing import Any, Callable, Optional

from . import process_manager as pm
from .meeturl import REFUSAL, is_meet_url
from .node_client import NodeClient
from .node_registry import NodeRegistry


def _str(description: str) -> dict[str, Any]:
    return {"type": "string", "description": description}


_NODE = _str("Name of an approved remote meet node to act on ('auto': the only one). Default: this machine.")


def _schema(name: str, description: str, properties: dict[str, Any], required=None) -> dict[str, Any]:
    params: dict[str, Any] = {"type": "object", "properties": properties, "additionalProperties": False}
    if required:
        params["required"] = required
    return {"name": name, "description": description, "parameters": params}


MEET_JOIN = _schema(
    "meet_join",
    "Join a Google Meet call and transcribe its live captions. Only https://meet.google.com/ URLs "
    "are accepted. The browser runs in the background and this returns at once: poll meet_status, "
    "read meet_transcript, finish with meet_leave. Announce yourself in the meeting: there is no "
    "automatic consent announcement.",
    {"url": _str("The full https://meet.google.com/... URL."),
     "mode": {"type": "string", "enum": ["transcribe", "realtime"],
              "description": "transcribe (default): listen only. realtime: also speak with meet_say "
                             "(needs an OpenAI key and a virtual microphone)."},
     "guest_name": _str("Display name when joining as a guest. Default 'Prometheus Agent'."),
     "duration": _str("Leave automatically after this long ('30m', '2h', '90s'). Default: until meet_leave."),
     "headed": {"type": "boolean", "description": "Show the browser window (debugging). Default false."},
     "node": _NODE},
    required=["url"])

MEET_STATUS = _schema(
    "meet_status",
    "The meeting bot's state: alive, joined, waiting in the lobby, caption lines captured, last "
    "caption time and microphone state.",
    {"node": _NODE})

MEET_TRANSCRIPT = _schema(
    "meet_transcript",
    "The captions transcribed so far ('last': only the last N lines).",
    {"last": {"type": "integer", "minimum": 1, "description": "Only the last N caption lines."},
     "node": _NODE})

MEET_SAY = _schema(
    "meet_say",
    "Speak text in the meeting (meetings joined with mode='realtime' only). It is queued and "
    "spoken a couple of seconds later.",
    {"text": _str("What to say."), "node": _NODE},
    required=["text"])

MEET_LEAVE = _schema(
    "meet_leave",
    "Leave the meeting, stop transcribing and finalize the transcript.",
    {"node": _NODE})


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False)


def _dispatch(node: Optional[str], remote: Callable[[NodeClient], dict], local: Callable[[], dict]) -> str:
    if not node:
        result = local()
        return _json({"success": bool(result.get("ok")), **result})
    entry = NodeRegistry().resolve(node)
    if entry is None:
        return _json({"success": False, "error": f"no approved meet node matches {node!r}: approve one "
                                                 "with `prometheus plugins run google_meet node approve`"})
    try:
        result = remote(NodeClient(entry["url"], entry["token"]))
    except Exception as exc:  # noqa: BLE001 - reported to the model
        return _json({"success": False, "error": f"remote node failed: {exc}", "node": entry["name"]})
    return _json({"success": bool(result.get("ok")), "node": entry["name"], **result})


def meet_join(args: dict[str, Any], session_id: str = "", **_: Any) -> str:
    url = str(args.get("url") or "").strip()
    if not is_meet_url(url):
        # Refused before any browser or node is involved.
        raise ValueError(REFUSAL)
    mode = str(args.get("mode") or "transcribe").strip().lower()
    if mode not in {"transcribe", "realtime"}:
        raise ValueError(f"mode must be 'transcribe' or 'realtime' (got {mode!r})")
    common = {"url": url, "guest_name": str(args.get("guest_name") or "Prometheus Agent"),
              "duration": str(args["duration"]) if args.get("duration") else None,
              "headed": bool(args.get("headed", False)), "mode": mode}
    return _dispatch(args.get("node"), lambda c: c.start_bot(**common),
                     lambda: pm.start(**common, session_id=session_id or None))


def meet_status(args: dict[str, Any], **_: Any) -> str:
    return _dispatch(args.get("node"), lambda c: c.status(), pm.status)


def meet_transcript(args: dict[str, Any], **_: Any) -> str:
    try:
        last = int(args["last"]) if args.get("last") is not None else None
    except (TypeError, ValueError):
        last = None
    last = last if last and last > 0 else None
    return _dispatch(args.get("node"), lambda c: c.transcript(last=last), lambda: pm.transcript(last=last))


def meet_say(args: dict[str, Any], **_: Any) -> str:
    text = str(args.get("text") or "").strip()
    if not text:
        raise ValueError("text is required")
    return _dispatch(args.get("node"), lambda c: c.say(text), lambda: pm.enqueue_say(text))


def meet_leave(args: dict[str, Any], **_: Any) -> str:
    return _dispatch(args.get("node"), lambda c: c.stop(), lambda: pm.stop(reason="meet_leave"))


TOOLS = (("meet_join", MEET_JOIN, meet_join), ("meet_status", MEET_STATUS, meet_status),
         ("meet_transcript", MEET_TRANSCRIPT, meet_transcript), ("meet_say", MEET_SAY, meet_say),
         ("meet_leave", MEET_LEAVE, meet_leave))
