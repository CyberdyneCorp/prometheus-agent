"""Hook payload translation: each callback gets only the keyword arguments
its signature declares (everything for ``**kwargs``), Hermes field names
next to Prometheus ones, the observer schema version, and placeholders
for live handles that cannot cross a process boundary."""

import inspect

TELEMETRY_SCHEMA_VERSION = "hermes.observer.v1"

# Prometheus payload field -> the name Hermes callbacks know it by.
ALIASES = {
    "tool": "tool_name",
    "arguments": "args",
    "call_id": "tool_call_id",
}

# Live objects a Hermes hook received in-process; the bridge sends a
# placeholder and marks the plugin degraded when a callback asks for one.
PLACEHOLDERS = {
    "pre_gateway_dispatch": ("gateway", "session_store"),
}


class Placeholder:
    """Stands for a live host object that stays in the host."""

    def __init__(self, name):
        self._name = name

    def __getattr__(self, attr):
        raise AttributeError(
            f"'{self._name}' is a placeholder: live gateway objects do not cross the "
            "Prometheus python bridge"
        )

    def __repr__(self):
        return f"<placeholder {self._name}>"


def payload_for(hook, fields):
    """The full keyword set of one firing."""
    kwargs = dict(fields)
    for ours, theirs in ALIASES.items():
        if ours in kwargs and theirs not in kwargs:
            kwargs[theirs] = kwargs[ours]
    for name in PLACEHOLDERS.get(hook, ()):
        kwargs.setdefault(name, Placeholder(name))
    kwargs.setdefault("telemetry_schema_version", TELEMETRY_SCHEMA_VERSION)
    return kwargs


def accepts(callback):
    """``None`` when the callback takes ``**kwargs``, else the names it declares."""
    try:
        params = inspect.signature(callback).parameters.values()
    except (TypeError, ValueError):
        return None
    if any(p.kind is inspect.Parameter.VAR_KEYWORD for p in params):
        return None
    return {p.name for p in params if p.kind in (inspect.Parameter.POSITIONAL_OR_KEYWORD, inspect.Parameter.KEYWORD_ONLY)}


def narrowed(callback, kwargs):
    """The keywords ``callback`` declares."""
    names = accepts(callback)
    if names is None:
        return kwargs
    return {k: v for k, v in kwargs.items() if k in names}


def wants_placeholder(hook, callback):
    """Whether ``callback`` asks for a live handle of ``hook``."""
    names = accepts(callback)
    wanted = PLACEHOLDERS.get(hook, ())
    return bool(wanted) and (names is None or any(n in names for n in wanted))
