"""Embed Prometheus in a Python program (spec: workspace-architecture,
Embeddable client SDKs).

The SDK speaks the daemon's versioned RPC protocol (``schema/rpc.json``)
with the standard library only::

    from prometheus_sdk import Client

    client = Client.connect()                      # attaches, or starts the daemon
    session = client.sessions.create(toolsets=["file"])
    for event in session.prompt("list files"):     # the protocol's events
        print(event.type)
    print(session.result.usage)                    # the turn's final result

``connect`` runs ``prometheus daemon ensure`` (attaching to the running
daemon or starting one), checks the daemon's protocol, then gets a
single-use ticket with ``prometheus daemon ticket`` (the ticketed loopback
flow every client uses). A daemon whose protocol major is not in
:data:`SUPPORTED_PROTOCOL_MAJORS` is refused with :class:`ProtocolMismatch`
before any ticket or request; so is a ``-32008`` answer to ``rpc.hello``. An
embedded session is an ordinary client session: the same capability
grants, approvals (``on_approval``) and budgets apply.
"""

from __future__ import annotations

import json
import queue
import subprocess
import threading
from dataclasses import dataclass, field
from typing import Any, Callable, Iterator, Optional

from ._websocket import WebSocket

__all__ = [
    "Client",
    "Event",
    "ProtocolMismatch",
    "Result",
    "RpcError",
    "Session",
    "SUPPORTED_PROTOCOL_MAJORS",
]

#: The protocol majors this SDK release speaks.
SUPPORTED_PROTOCOL_MAJORS: tuple[int, ...] = (1,)
#: The minor it was written against.
PROTOCOL_MINOR = 0
#: This package's version.
VERSION = "0.1.0"
#: Seconds between two ``rpc.ping`` (the daemon's limit is 45).
HEARTBEAT_SECS = 15
#: The JSON-RPC error code of a protocol mismatch at ``rpc.hello``.
PROTOCOL_MISMATCH_CODE = -32008


class RpcError(Exception):
    """A JSON-RPC error answer."""

    def __init__(self, error: dict[str, Any]) -> None:
        self.code = int(error.get("code", 0))
        self.data = error.get("data")
        super().__init__(error.get("message", "rpc error"))


class ProtocolMismatch(Exception):
    """The daemon speaks a protocol major this SDK does not support."""


@dataclass
class Event:
    """One event of a turn: a trajectory event (``tool.call``,
    ``tool.result``, ``assistant.message``...) with its fields."""

    type: str
    data: dict[str, Any]


@dataclass
class Result:
    """How a turn ended (``turn.finished``)."""

    exit_reason: str
    final_response: Optional[str]
    usage: dict[str, Any] = field(default_factory=dict)
    error: Optional[dict[str, Any]] = None


Approval = Callable[[dict[str, Any]], str]
Clarify = Callable[[dict[str, Any]], Any]


def _cli(binary: str, home: Optional[str], profile: Optional[str], *command: str) -> dict[str, Any]:
    """``prometheus --json <command>``'s answer."""
    args = [binary]
    if home:
        args += ["--home", home]
    if profile:
        args += ["--profile", profile]
    args += ["--json", *command]
    out = subprocess.run(args, capture_output=True, text=True, check=False, stdin=subprocess.DEVNULL)
    if out.returncode != 0:
        raise ConnectionError(f"prometheus {' '.join(command)} failed: {out.stderr.strip()}")
    return json.loads(out.stdout)


def check_protocol(version: str) -> int:
    """The major of ``<major>.<minor>``, refused when unsupported."""
    head = version.split(".")[0]
    major = int(head) if head.isdigit() else -1
    if major not in SUPPORTED_PROTOCOL_MAJORS:
        supported = ", ".join(str(m) for m in SUPPORTED_PROTOCOL_MAJORS)
        raise ProtocolMismatch(f"the daemon speaks protocol {version}; this SDK supports major {supported}")
    return major


class Client:
    """A connection to the daemon."""

    def __init__(self, ws: WebSocket, profile: str, on_approval: Optional[Approval], on_clarify: Optional[Clarify]):
        self._ws = ws
        self.profile = profile
        self.on_approval = on_approval
        self.on_clarify = on_clarify
        self._next = 0
        self._lock = threading.Lock()
        self._pending: dict[int, queue.Queue] = {}
        self._events: dict[str, queue.Queue] = {}
        self.hello: dict[str, Any] = {}
        self.sessions = Sessions(self)
        self._closed = threading.Event()
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()
        # The daemon drops a connection silent for 45 s.
        threading.Thread(target=self._heartbeat, daemon=True).start()

    def _heartbeat(self) -> None:
        while not self._closed.wait(HEARTBEAT_SECS):
            try:
                self.call("rpc.ping", {}, timeout=HEARTBEAT_SECS)
            except Exception:  # noqa: BLE001 - the reader reports the closed connection
                return

    @classmethod
    def connect(
        cls,
        home: Optional[str] = None,
        profile: Optional[str] = None,
        *,
        binary: str = "prometheus",
        on_approval: Optional[Approval] = None,
        on_clarify: Optional[Clarify] = None,
    ) -> "Client":
        """Attaches to the daemon of ``home`` (starting it when needed)
        for ``profile`` (default ``default``). ``on_approval`` answers an
        approval request's params with ``once``, ``session``, ``always`` or
        ``deny``; ``on_clarify`` answers clarifying questions with one
        answer per question."""
        endpoint = _cli(binary, home, profile, "daemon", "ensure")
        major = check_protocol(endpoint["rpc_protocol"])
        ticket = _cli(binary, home, profile, "daemon", "ticket")["ticket"]
        ws = WebSocket(endpoint["rpc_url"], f"prometheus-rpc.v{major}, prometheus-ticket.{ticket}")
        client = cls(ws, profile or "default", on_approval, on_clarify)
        client._hello(major)
        return client

    def _hello(self, major: int) -> None:
        # Without callbacks the daemon resolves its requests unanswered.
        answers = self.on_approval is not None or self.on_clarify is not None
        params = {
            "client": "cli",
            "client_version": VERSION,
            "protocol": {"major": major, "minor": PROTOCOL_MINOR},
            "capabilities": {"server_requests": answers, "events": []},
        }
        try:
            self.hello = self.call("rpc.hello", params)
        except RpcError as err:
            if err.code == PROTOCOL_MISMATCH_CODE:
                self.close()
                raise ProtocolMismatch(str(err)) from err
            raise
        protocol = self.hello.get("protocol", {})
        check_protocol(f"{protocol.get('major')}.{protocol.get('minor', 0)}")

    def call(self, method: str, params: Optional[dict[str, Any]] = None, timeout: float = 300) -> Any:
        """One request; its result, or :class:`RpcError`."""
        with self._lock:
            self._next += 1
            ident = self._next
            answer: queue.Queue = queue.Queue(1)
            self._pending[ident] = answer
        self._ws.send(json.dumps({"jsonrpc": "2.0", "id": ident, "method": method, "params": params or {}}))
        reply = answer.get(timeout=timeout)
        if "error" in reply:
            raise RpcError(reply["error"])
        return reply.get("result")

    def _read(self) -> None:
        while True:
            text = self._ws.recv()
            if text is None:
                break
            frame = json.loads(text)
            if "method" in frame and "id" in frame:
                threading.Thread(target=self._answer, args=(frame,), daemon=True).start()
            elif "method" in frame:
                envelope = frame.get("params") or {}
                session = envelope.get("session_id") or ""
                self._queue_of(session).put((envelope.get("event", ""), envelope.get("data") or {}))
            elif frame.get("id") in self._pending:
                self._pending.pop(frame["id"]).put(frame)
        for answer in list(self._pending.values()):
            answer.put({"error": {"code": -32000, "message": "connection closed"}})
        for events in list(self._events.values()):
            events.put(None)
        self._closed.set()

    def _answer(self, request: dict[str, Any]) -> None:
        """A server request (an approval or a clarifying question)."""
        params = request.get("params") or {}
        if request["method"] == "approval.request" and self.on_approval:
            result: Any = {"decision": self.on_approval(params)}
        elif request["method"] == "clarify.request" and self.on_clarify:
            result = {"answers": list(self.on_clarify(params))}
        else:
            error = {"code": -32601, "message": f"{request['method']} is not handled by this client"}
            self._ws.send(json.dumps({"jsonrpc": "2.0", "id": request["id"], "error": error}))
            return
        self._ws.send(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}))

    def _queue_of(self, session: str) -> queue.Queue:
        with self._lock:
            return self._events.setdefault(session, queue.Queue())

    def close(self) -> None:
        self._closed.set()
        self._ws.close()


class Sessions:
    """``client.sessions``."""

    def __init__(self, client: Client) -> None:
        self._client = client

    def create(
        self,
        toolsets: Optional[list[str]] = None,
        model: Optional[str] = None,
        system_prompt_overlay: Optional[str] = None,
        ephemeral: bool = False,
    ) -> "Session":
        """A new session of the client's profile, attached to this client."""
        setup: dict[str, Any] = {"ephemeral": ephemeral}
        if toolsets is not None:
            setup["set"] = [f"tools.enabled_toolsets={json.dumps(toolsets)}"]
        if model:
            setup["model"] = model
        if system_prompt_overlay:
            setup["system_prompt_overlay"] = system_prompt_overlay
        created = self._client.call("session.create", {"setup": setup})
        # The creating connection is attached to the session: its events
        # reach this client.
        return Session(self._client, created["session_id"])


class Session:
    """One session: ``prompt``, ``interrupt`` and ``history``."""

    def __init__(self, client: Client, session_id: str) -> None:
        self._client = client
        self.id = session_id
        self.result: Optional[Result] = None

    def prompt(self, text: str, attachments: Optional[list[str]] = None) -> Iterator[Event]:
        """Runs one turn; yields its events, then sets :attr:`result`."""
        events = self._client._queue_of(self.id)
        params: dict[str, Any] = {"session_id": self.id, "text": text}
        if attachments:
            params["files"] = attachments
        admitted = self._client.call("prompt.submit", params)
        turn = admitted.get("turn_id")
        while True:
            item = events.get()
            if item is None:
                raise ConnectionError("the daemon closed the connection")
            name, data = item
            if name == "turn.finished" and (turn is None or data.get("turn_id") == turn):
                self.result = Result(
                    exit_reason=data.get("exit_reason", ""),
                    final_response=data.get("final_response"),
                    usage=data.get("usage") or {},
                    error=data.get("error"),
                )
                return
            if name == "session.event":
                yield Event(type=str(data.get("type", "")), data=data)

    def interrupt(self) -> None:
        """Stops the running turn at its next safe point."""
        self._client.call("session.interrupt", {"session_id": self.id})

    def history(self) -> list[dict[str, Any]]:
        """The session's messages so far (multi-turn use)."""
        transcript = self._client.call("session.transcript", {"session_id": self.id})
        return transcript.get("messages", [])
