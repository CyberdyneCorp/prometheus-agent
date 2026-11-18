"""The NeMo Relay host (spec: observability, NeMo Relay and Langfuse
integrations).

The process that enables Relay starts this module once (the Relay policy is
daemon-wide) in a managed environment holding ``nemo-relay``, and feeds it
the session, turn, model-call and tool lifecycle of every session from the
trajectory stream, one JSON object per stdin line:

- ``{"op": "start", "plugins_toml": <path or null>}`` first: Relay loads its
  user ``plugins.toml`` (replaced by ``plugins_toml`` when given) and then
  its system file, which has the highest precedence;
- ``session_start`` / ``session_end``, ``turn_start`` / ``turn_end``,
  ``tool_start`` / ``tool_end`` push and pop Relay scopes (Agent for a
  session, Function for a turn, Tool for a call);
- ``llm`` records one finished model call as an Llm scope pushed and
  popped at once; ``message`` marks a user or assistant message;
- ``flush`` drains Relay's subscribers (end of input flushes too).

Payloads (``input``, ``output``, message ``text``) are present only when the
host process captures content (``observability.otel.capture_content``).
One answer line follows ``start`` (``{"ok": true}`` or ``{"error": ...}``);
nothing else is answered, and a failing event is reported on stderr and
skipped: a broken exporter never stops the agent.

Each session gets a scope stack of its own (its own ``contextvars``
context), so concurrent sessions never pop each other's scopes.
"""

from __future__ import annotations

import asyncio
import contextvars
import inspect
import json
import sys
from typing import Any

SESSION_SCOPE = "prometheus.session"
TURN_SCOPE = "prometheus.turn"

LOOP = asyncio.new_event_loop()


def _settle(value: Any) -> Any:
    """Relay calls may be coroutines; run them on the host's one loop."""
    if inspect.isawaitable(value):
        return LOOP.run_until_complete(value)
    return value


class Host:
    """Open Relay scopes by session, turn and tool call."""

    def __init__(self, relay: Any) -> None:
        self.relay = relay
        self.contexts: dict[str, contextvars.Context] = {}
        self.sessions: dict[str, Any] = {}
        self.turns: dict[tuple[str, str], Any] = {}
        self.tools: dict[tuple[str, str], Any] = {}
        self.activation: Any = None

    def _kind(self, name: str) -> Any:
        return getattr(self.relay.ScopeType, name)

    def push(self, name: str, kind: str, parent: Any, event: dict[str, Any]) -> Any:
        kwargs: dict[str, Any] = {"metadata": event.get("metadata") or {}}
        if parent is not None:
            kwargs["handle"] = parent
        if "input" in event:
            kwargs["input"] = event["input"]
        return self.relay.scope.push(name, self._kind(kind), **kwargs)

    def pop(self, handle: Any, event: dict[str, Any]) -> None:
        if handle is None:
            return
        kwargs: dict[str, Any] = {"metadata": event.get("metadata") or {}}
        if "output" in event:
            kwargs["output"] = event["output"]
        self.relay.scope.pop(handle, **kwargs)

    def parent(self, session: str, event: dict[str, Any]) -> Any:
        return self.turns.get((session, str(event.get("turn", "")))) or self.sessions.get(session)

    def handle(self, event: dict[str, Any]) -> None:
        op = event.get("op")
        if op == "flush":
            self.relay.subscribers.flush()
            return
        session = str(event.get("session", ""))
        context = self.contexts.setdefault(session, contextvars.Context())
        context.run(self._session_op, op, session, event)
        if op == "session_end":
            self.contexts.pop(session, None)

    def _session_op(self, op: str, session: str, event: dict[str, Any]) -> None:
        if op == "session_start":
            self.sessions[session] = self.push(SESSION_SCOPE, "Agent", None, event)
        elif op == "turn_start":
            self.turns[(session, str(event["turn"]))] = self.push(
                TURN_SCOPE, "Function", self.sessions.get(session), event
            )
        elif op == "llm":
            self.pop(self.push(str(event.get("name", "model")), "Llm", self.parent(session, event), event), event)
        elif op == "message":
            self.relay.scope.event(
                f"{event['role']}.message", handle=self.parent(session, event), data={"text": event["text"]}
            )
        elif op == "tool_start":
            self.tools[(session, str(event["call"]))] = self.push(
                str(event["name"]), "Tool", self.parent(session, event), event
            )
        elif op == "tool_end":
            self.pop(self.tools.pop((session, str(event["call"])), None), event)
        elif op == "turn_end":
            self.pop(self.turns.pop((session, str(event["turn"])), None), event)
        elif op == "session_end":
            self.end(session, event)

    def end(self, session: str, event: dict[str, Any]) -> None:
        """Closes the session's scopes, innermost first."""
        for key in [k for k in self.tools if k[0] == session]:
            self.pop(self.tools.pop(key), {})
        for key in [k for k in self.turns if k[0] == session]:
            self.pop(self.turns.pop(key), {})
        self.pop(self.sessions.pop(session, None), event)


def _answer(value: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def main() -> int:
    first = json.loads(sys.stdin.readline() or "{}")
    try:
        import nemo_relay as relay  # noqa: PLC0415 - the managed environment's package, loaded here only

        # Held for the process's life: the activation keeps the plugins on.
        activation = _settle(relay.plugin.initialize({}, additional_plugins_toml=first.get("plugins_toml")))
    except Exception as exc:  # noqa: BLE001 - reported to the parent, which logs it
        _answer({"error": f"{type(exc).__name__}: {exc}"})
        return 1
    _answer({"ok": True})
    host = Host(relay)
    host.activation = activation
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            host.handle(json.loads(line))
        except Exception as exc:  # noqa: BLE001 - one bad event never stops the host
            print(f"relay event failed: {type(exc).__name__}: {exc}", file=sys.stderr, flush=True)
    for session in list(host.sessions):
        host.contexts.get(session, contextvars.Context()).run(host.end, session, {})
    host.handle({"op": "flush"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
