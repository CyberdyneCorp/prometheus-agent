"""A Hermes ``MemoryProvider`` driven with the calls of the host's
``memory-provider`` world (spec: python-bridge, Scope of hosted code; memory,
External memory provider contract). A method the provider does not define is
a no-op, as an omitted optional function of the world is."""


def _schema(spec):
    """``{"name", "description", "parameters"}`` of a Hermes tool schema
    (plain, or OpenAI's ``{"type": "function", "function": {...}}``)."""
    fn = spec.get("function", spec) if isinstance(spec, dict) else {}
    return {"name": fn.get("name", ""), "description": fn.get("description", ""),
            "parameters": fn.get("parameters") or {"type": "object"}}


def _optional(target, name, *args, **kwargs):
    fn = getattr(target, name, None)
    return fn(*args, **kwargs) if callable(fn) else None


def _write(target, args):
    metadata = {"entry_id": args.get("entry_id", ""), "previous_content": args.get("previous_content")}
    target.on_memory_write(args.get("action", ""), args.get("target", ""), args.get("content", ""), metadata)


def _status(target):
    status = _optional(target, "recall_status")
    return f"{status.provider_label}: {status.count}" if status else None


CALLS = {
    "name": lambda t, a: t.name,
    "is_available": lambda t, a: bool(t.is_available()),
    "unavailable_reason": lambda t, a: t.unavailable_reason() or None,
    "initialize": lambda t, a: t.initialize(a.get("session_id", ""), profile=a.get("profile", ""),
                                            platform=a.get("platform", ""),
                                            **({"user_id": a["user_id"]} if a.get("user_id") else {})),
    "system_prompt_block": lambda t, a: t.system_prompt_block() or None,
    "prefetch": lambda t, a: t.prefetch(a.get("query", ""), session_id=a.get("session_id", "")) or None,
    "queue_prefetch": lambda t, a: t.queue_prefetch(a.get("query", ""), session_id=a.get("session_id", "")),
    "sync_turn": lambda t, a: t.sync_turn(a.get("user", ""), a.get("assistant", ""),
                                          session_id=a.get("session_id", "")),
    "tool_schemas": lambda t, a: [_schema(s) for s in (t.get_tool_schemas() or [])],
    "handle_tool_call": lambda t, a: str(t.handle_tool_call(a.get("name", ""), a.get("arguments") or {})),
    "on_turn_start": lambda t, a: t.on_turn_start(a.get("turn", 0), a.get("text", "")),
    "on_session_end": lambda t, a: t.on_session_end([]),
    "on_session_switch": lambda t, a: _optional(t, "on_session_switch", a.get("session_id", "")),
    "on_pre_compress": lambda t, a: bool(t.on_pre_compress(a.get("messages") or [])),
    "on_delegation": lambda t, a: _optional(t, "on_delegation", a.get("task", "")),
    "on_memory_write": _write,
    "config_schema": lambda t, a: _optional(t, "get_config_schema") or [],
    "post_setup": lambda t, a: _optional(t, "post_setup"),
    "recall_status": lambda t, a: _status(t),
}


def call(target, method, args):
    fn = CALLS.get(method)
    if fn is None:
        raise ValueError(f"unknown memory provider call '{method}'")
    return fn(target, args)
