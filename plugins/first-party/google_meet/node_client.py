"""The profile side of a remote meet node: one short WebSocket per call."""

from __future__ import annotations

from typing import Any, Optional

from . import node_protocol as proto
from . import runtime


class NodeClient:
    def __init__(self, url: str, token: str, timeout: float = 10.0) -> None:
        self.url = url
        self.token = token
        self.timeout = timeout

    def _rpc(self, kind: str, payload: dict[str, Any]) -> dict[str, Any]:
        connect = runtime.import_from_runtime("websockets.sync.client").connect
        req = proto.request(kind, self.token, payload)
        with connect(self.url, open_timeout=self.timeout, close_timeout=self.timeout) as ws:
            ws.send(proto.encode(req))
            resp = proto.decode(ws.recv(timeout=self.timeout))
        if resp.get("type") == "error":
            raise RuntimeError(f"node error: {resp.get('error', '<unknown>')}")
        if resp.get("id") != req["id"]:
            raise RuntimeError(f"response id mismatch: sent {req['id']}, got {resp.get('id')!r}")
        if not isinstance(resp.get("payload"), dict):
            raise RuntimeError("response missing payload")
        return resp["payload"]

    def start_bot(self, url: str, guest_name: str, duration: Optional[str], headed: bool,
                  mode: str) -> dict[str, Any]:
        payload: dict[str, Any] = {"url": url, "guest_name": guest_name, "headed": headed, "mode": mode}
        if duration:
            payload["duration"] = duration
        return self._rpc("start_bot", payload)

    def stop(self) -> dict[str, Any]:
        return self._rpc("stop", {})

    def status(self) -> dict[str, Any]:
        return self._rpc("status", {})

    def transcript(self, last: Optional[int] = None) -> dict[str, Any]:
        return self._rpc("transcript", {} if last is None else {"last": int(last)})

    def say(self, text: str) -> dict[str, Any]:
        return self._rpc("say", {"text": text})

    def ping(self) -> dict[str, Any]:
        return self._rpc("ping", {})
