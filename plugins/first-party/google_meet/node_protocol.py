"""The JSON envelopes between a profile and a remote meet node.

    Request:  {"type": <str>, "id": <str>, "token": <str>, "payload": <dict>}
    Response: {"type": "response", "id": <request id>, "payload": <dict>}
    Error:    {"type": "error", "id": <request id>, "error": <str>}

Requests carry the node's shared token (``node approve`` stores it on the
profile side; the node keeps it in its plugin data); a wrong token is
refused before dispatch.
"""

from __future__ import annotations

import json
import uuid
from typing import Any

REQUEST_TYPES = frozenset({"start_bot", "stop", "status", "transcript", "say", "ping"})


def request(kind: str, token: str, payload: dict[str, Any]) -> dict[str, Any]:
    if kind not in REQUEST_TYPES:
        raise ValueError(f"unknown request type: {kind!r}")
    return {"type": kind, "id": uuid.uuid4().hex, "token": token, "payload": payload}


def response(req_id: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {"type": "response", "id": req_id, "payload": payload}


def error(req_id: str, message: str) -> dict[str, Any]:
    return {"type": "error", "id": req_id, "error": str(message)}


def encode(message: dict[str, Any]) -> str:
    return json.dumps(message, separators=(",", ":"), ensure_ascii=False)


def decode(raw) -> dict[str, Any]:
    """An envelope with string ``type`` and ``id``; ValueError otherwise."""
    if isinstance(raw, (bytes, bytearray)):
        raw = raw.decode("utf-8")
    try:
        message = json.loads(raw)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"malformed JSON: {exc}") from exc
    if not isinstance(message, dict):
        raise ValueError("envelope must be a JSON object")
    for key in ("type", "id"):
        if not isinstance(message.get(key), str):
            raise ValueError(f"envelope missing string '{key}'")
    return message


def check(message: dict[str, Any], token: str) -> str:
    """Why ``message`` is refused, or ``""``."""
    kind = message.get("type")
    checks = ((kind in REQUEST_TYPES, f"unknown request type: {kind!r}"),
              (isinstance(message.get("id"), str) and bool(message.get("id")), "missing 'id'"),
              (isinstance(message.get("token"), str) and bool(message.get("token")), "missing token"),
              (message.get("token") == token, "token mismatch"),
              (isinstance(message.get("payload"), dict), "payload must be an object"))
    return next((reason for ok, reason in checks if not ok), "")
