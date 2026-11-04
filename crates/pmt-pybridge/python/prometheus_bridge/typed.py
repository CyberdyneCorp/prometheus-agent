"""Typed values across the wire: arguments and results are rebuilt from the
annotations of the method being called (dataclasses, enums, pathlib.Path,
lists, tuples, dicts and optionals); unannotated values pass as JSON data."""

import dataclasses
import enum
import inspect
import pathlib
import typing


class Unserializable(Exception):
    """A return value with no JSON form."""


def to_wire(value, method="<call>"):
    """``value`` as JSON data, or Unserializable naming its type and ``method``."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, enum.Enum):
        return to_wire(value.value, method)
    if isinstance(value, pathlib.PurePath):
        return str(value)
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: to_wire(getattr(value, f.name), method) for f in dataclasses.fields(value)}
    if isinstance(value, dict):
        return {str(k): to_wire(v, method) for k, v in value.items()}
    if isinstance(value, (list, tuple, set, frozenset)):
        return [to_wire(v, method) for v in value]
    if isinstance(value, bytes):
        try:
            return value.decode("utf-8")
        except UnicodeDecodeError:
            pass
    raise Unserializable(
        f"unserializable return value of type {type(value).__name__} from {method}"
    )


def from_wire(value, annotation):
    """``value`` rebuilt as ``annotation`` (left as is when it does not apply)."""
    if annotation in (inspect.Parameter.empty, typing.Any, None) or value is None:
        return value
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin is typing.Union or (origin is not None and str(origin) == "<class 'types.UnionType'>"):
        for arg in args:
            if arg is type(None):
                continue
            try:
                return from_wire(value, arg)
            except (TypeError, ValueError):
                continue
        return value
    if origin in (list, tuple, set, frozenset) and isinstance(value, list):
        inner = args[0] if args else typing.Any
        items = [from_wire(v, inner) for v in value]
        return origin(items) if origin is not list else items
    if origin is dict and isinstance(value, dict):
        inner = args[1] if len(args) == 2 else typing.Any
        return {k: from_wire(v, inner) for k, v in value.items()}
    if isinstance(annotation, type):
        if issubclass(annotation, enum.Enum):
            return annotation(value)
        if issubclass(annotation, pathlib.PurePath):
            return annotation(value)
        if dataclasses.is_dataclass(annotation) and isinstance(value, dict):
            hints = _hints(annotation)
            fields = {f.name for f in dataclasses.fields(annotation)}
            return annotation(**{k: from_wire(v, hints.get(k, typing.Any)) for k, v in value.items() if k in fields})
    return value


def _hints(obj):
    try:
        return typing.get_type_hints(obj)
    except Exception:  # noqa: BLE001 - unresolvable annotations stay untyped
        return {}


def call(method, args, name):
    """Calls ``method`` with JSON ``args`` (a dict of keywords or a list of
    positionals) rebuilt per its annotations; returns its result as JSON."""
    hints = _hints(method)
    try:
        signature = inspect.signature(method)
    except (TypeError, ValueError):
        signature = None
    if isinstance(args, dict):
        kwargs = {k: from_wire(v, hints.get(k, typing.Any)) for k, v in args.items()}
        result = method(**kwargs)
    else:
        params = list(signature.parameters) if signature else []
        positional = [from_wire(v, hints.get(params[i], typing.Any) if i < len(params) else typing.Any)
                      for i, v in enumerate(args or [])]
        result = method(*positional)
    if inspect.iscoroutine(result):
        import asyncio
        result = asyncio.run(result)
    return to_wire(result, name)
