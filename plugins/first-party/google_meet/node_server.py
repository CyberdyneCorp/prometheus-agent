"""A remote meet node (``prometheus plugins run google_meet node run``): a
WebSocket server running the bot on this machine for a profile elsewhere
(the signed-in browser profile stays here). Its token is minted on first
start and kept in the plugin's data directory."""

from __future__ import annotations

import asyncio
import secrets
import time
from typing import Any, Callable

from . import node_protocol as proto
from . import process_manager as pm
from . import runtime
from .jsonfile import read_json, write_json

_START_KEYS = ("url", "guest_name", "duration", "headed", "mode")

_HANDLERS: dict[str, Callable[[dict[str, Any]], dict[str, Any]]] = {
    "start_bot": lambda p: pm.start(**{k: p[k] for k in _START_KEYS if k in p}),
    "stop": lambda p: pm.stop(reason=str(p.get("reason") or "requested by the profile")),
    "status": lambda p: pm.status(),
    "transcript": lambda p: pm.transcript(last=p.get("last")),
    "say": lambda p: pm.enqueue_say(str(p.get("text") or "")),
}


class NodeServer:
    def __init__(self, host: str, port: int, display_name: str) -> None:
        self.host = host
        self.port = port
        self.display_name = display_name
        self.token_path = runtime.paths().node_token

    def token(self) -> str:
        """The shared token, minted on first use."""
        data = read_json(self.token_path)
        token = data.get("token") if isinstance(data, dict) else None
        if not (isinstance(token, str) and token):
            token = secrets.token_hex(16)
            write_json(self.token_path, {"token": token, "generated_at": time.time()})
        return token

    def handle(self, message: dict[str, Any]) -> dict[str, Any]:
        """One request's answer envelope; never raises."""
        refusal = proto.check(message, self.token())
        if refusal:
            return proto.error(str(message.get("id") or ""), refusal)
        req_id, kind = message["id"], message["type"]
        if kind == "ping":
            return {"type": "pong", "id": req_id,
                    "payload": {"display_name": self.display_name, "ts": time.time()}}
        if kind == "start_bot" and "url" not in message["payload"]:
            return proto.error(req_id, "missing 'url' in payload")
        try:
            return proto.response(req_id, _HANDLERS[kind](message["payload"]))
        except Exception as exc:  # noqa: BLE001 - reported to the caller
            return proto.error(req_id, f"{type(exc).__name__}: {exc}")

    async def serve(self) -> None:
        websockets = runtime.import_from_runtime("websockets")
        self.token()

        async def on_connection(ws):
            async for raw in ws:
                try:
                    message = proto.decode(raw)
                except ValueError as exc:
                    await ws.send(proto.encode(proto.error("", f"decode: {exc}")))
                    continue
                await ws.send(proto.encode(self.handle(message)))

        async with websockets.serve(on_connection, self.host, self.port):
            await asyncio.Future()
