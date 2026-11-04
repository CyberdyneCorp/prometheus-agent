"""The sidecar loop: one plugin, one profile, JSON-RPC over stdin/stdout."""

import asyncio
import importlib.util
import inspect
import itertools
import json
import os
import sys
import threading
import traceback

from . import PROTOCOL_VERSION, cli, context, hooks, imports, state, typed
from .wire import FrameTooLarge, Wire

UNKNOWN_METHOD = -32601
INVALID_PARAMS = -32602
PLUGIN_ERROR = -32000
FRAME_TOO_LARGE = -32001
UNSUPPORTED = -32010
UNSERIALIZABLE = -32011

METHODS = {"bridge.hello", "bridge.ping", "bridge.shutdown", "plugin.load", "plugin.unload", "tool.invoke",
           "hook.invoke", "command.invoke", "provider.call", "event.deliver"}


class Failure(Exception):
    def __init__(self, code, message, data=None):
        super().__init__(message)
        self.code = code
        self.data = data or {}


class Sidecar:
    def __init__(self, wire, real_stderr):
        self.wire = wire
        self.real_stderr = real_stderr
        self.ids = itertools.count(1)
        self.pending = {}
        self.lock = threading.Lock()
        self.ctx = None

    # Requests to the host.
    def host_call(self, method, args, timeout):
        call_id = f"s{next(self.ids)}"
        done = threading.Event()
        slot = {}
        with self.lock:
            self.pending[call_id] = (done, slot)
        self.wire.write({"jsonrpc": "2.0", "id": call_id, "method": "host.call",
                         "params": {"method": method, "args": args, "context": state.current_call()}})
        if not done.wait(timeout):
            with self.lock:
                self.pending.pop(call_id, None)
            raise context.HostError("timeout", f"host did not answer {method}")
        if "error" in slot:
            error = slot["error"]
            raise context.HostError((error.get("data") or {}).get("code", ""), error.get("message", ""))
        return slot.get("result")

    def _resolve(self, message):
        with self.lock:
            entry = self.pending.pop(message.get("id"), None)
        if entry is None:
            return
        done, slot = entry
        if "error" in message:
            slot["error"] = message["error"]
        else:
            slot["result"] = message.get("result")
        done.set()

    # Answers.
    def respond(self, call_id, result=None, failure=None):
        message = {"jsonrpc": "2.0", "id": call_id}
        if failure is None:
            message["result"] = result
        else:
            message["error"] = {"code": failure.code, "message": str(failure), "data": failure.data}
        try:
            self.wire.write(message)
        except FrameTooLarge as err:
            self.wire.write({"jsonrpc": "2.0", "id": call_id,
                             "error": {"code": FRAME_TOO_LARGE, "message": str(err), "data": {"code": "frame_too_large"}}})

    def run(self):
        while True:
            message = self.wire.read()
            if message is None:
                # The host is gone: clean up and take every child along.
                self._unload()
                _end_group()
                return 0
            if "method" not in message:
                self._resolve(message)
                continue
            method, call_id = message["method"], message.get("id")
            if method not in METHODS:
                self.respond(call_id, failure=Failure(UNKNOWN_METHOD, f"unknown method '{method}'"))
            elif method == "bridge.ping":
                self.respond(call_id, {})
            elif method == "bridge.shutdown":
                self._unload()
                self.respond(call_id, {})
                return 0
            elif method == "bridge.hello":
                self.respond(call_id, self.hello(message.get("params") or {}))
            else:
                threading.Thread(target=self._serve, args=(method, call_id, message.get("params") or {}),
                                 daemon=True).start()

    def _serve(self, method, call_id, params):
        state.set_call(params.get("context"))
        try:
            result = HANDLERS[method](self, params)
            self.respond(call_id, result)
        except Failure as failure:
            self.respond(call_id, failure=failure)
        except context.UnsupportedPluginApi as err:
            self.respond(call_id, failure=Failure(UNSUPPORTED, str(err), {"code": "unsupported_api"}))
        except typed.Unserializable as err:
            self.respond(call_id, failure=Failure(UNSERIALIZABLE, str(err), {"code": "unserializable"}))
        except Exception as err:  # noqa: BLE001 - a plugin error is the call's answer
            traceback.print_exc(file=self.real_stderr)
            self.respond(call_id, failure=Failure(PLUGIN_ERROR, f"{type(err).__name__}: {err}",
                                                  {"code": "plugin_error"}))

    def hello(self, params):
        return {"protocol_version": PROTOCOL_VERSION, "compat_level": params.get("compat_level", ""),
                "plugin_id": params.get("plugin_id", ""), "python": sys.version.split()[0]}

    def _unload(self):
        for callback in reversed(state.REGISTRY["unload"]):
            try:
                callback()
            except Exception:  # noqa: BLE001 - unload callbacks never propagate
                traceback.print_exc(file=self.real_stderr)
        state.REGISTRY["unload"].clear()


def _end_group():
    """Kills this sidecar's process group (it leads it) on Unix."""
    if hasattr(os, "killpg"):
        try:
            os.killpg(os.getpgid(0), 9)
        except OSError:
            pass


def _load(sidecar, params):
    state.PLUGIN_ID = params["plugin_id"]
    state.PROFILE = params.get("profile", "")
    state.GRANTED_SECRETS = frozenset(params.get("secrets", ()))
    state.CAPABILITIES = frozenset(params.get("capabilities", ()))
    state.DATA_DIR = params.get("data_dir", "")
    state.API = params.get("api", {})
    imports.install()
    directory = params["dir"]
    init = os.path.join(directory, "__init__.py")
    module_name = "prometheus_plugin_" + state.PLUGIN_ID.replace("-", "_")
    spec = importlib.util.spec_from_file_location(module_name, init, submodule_search_locations=[directory])
    if spec is None or spec.loader is None:
        raise Failure(PLUGIN_ERROR, f"plugin '{state.PLUGIN_ID}' has no __init__.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    sidecar.ctx = context.PluginContext(params.get("manifest", {}))
    register = getattr(module, "register", None)
    if callable(register):
        register(sidecar.ctx)
    return registrations()


def registrations():
    reg = state.REGISTRY
    sections = []
    for section_id, section in reg["sections"].items():
        static = section["content"] if isinstance(section["content"], str) else None
        sections.append({"id": section_id, "max_chars": section["max_chars"], "static": static})
    return {
        "tools": [{k: typed.to_wire(v) for k, v in t.items() if k not in ("handler", "check_fn")}
                  for t in reg["tools"].values()],
        "hooks": sorted(reg["hooks"]),
        "sections": sections,
        "commands": [{"name": n, "description": c["description"], "args_hint": c["args_hint"]}
                     for n, c in reg["commands"].items()],
        "cli_commands": [{"name": n, "help": c["help"], "description": c["description"]}
                         for n, c in reg["cli_commands"].items()],
        "providers": [_profile(p) for p in reg["providers"].values()],
        "provider_objects": _objects(reg["provider_objects"]),
        "memory_providers": sorted(reg["memory_providers"]),
        "context_engines": sorted(reg["context_engines"]),
        "transports": sorted(reg["transports"]),
        "subscriptions": [event for event, _ in reg["subscriptions"]],
        "skills": [{"name": n, **s} for n, s in reg["skills"].items()],
        "degraded": list(dict.fromkeys(reg["degraded"])),
    }


def _objects(objects):
    """Provider names by kind; a web search provider that extracts is also a web_extract provider."""
    out = {k: sorted(v) for k, v in objects.items()}
    extract = [n for n, p in objects.get("web_search", {}).items()
               if callable(getattr(p, "supports_extract", None)) and p.supports_extract()]
    if extract:
        out["web_extract"] = sorted(extract)
    return out


PROFILE_FIELDS = ("name", "aliases", "api_mode", "env_vars", "base_url", "models_url", "auth_type", "display_name",
                  "description", "default_headers", "fallback_models", "supports_model_listing")
PROFILE_HOOKS = ("prepare_messages", "build_extra_body", "build_api_kwargs_extras", "build_client_kwargs_extras",
                 "fetch_models")


def _profile(profile):
    from providers.base import ProviderProfile

    out = {f: typed.to_wire(getattr(profile, f, None)) for f in PROFILE_FIELDS}
    out["hooks"] = [h for h in PROFILE_HOOKS
                    if getattr(type(profile), h, None) is not getattr(ProviderProfile, h, None)]
    if getattr(profile, "classify_api_error", None):
        out["hooks"].append("classify_api_error")
    return out


def _tool(_, params):
    entry = state.REGISTRY["tools"].get(params["name"])
    if entry is None:
        raise Failure(PLUGIN_ERROR, f"tool '{params['name']}' is no longer provided by plugin '{state.PLUGIN_ID}'",
                      {"code": "tool_gone"})
    handler = entry["handler"]
    call = state.current_call()
    extra = hooks.narrowed(handler, {"task_id": call.get("session_id", ""), "session_id": call.get("session_id", ""),
                                     "tool_call_id": call.get("call_id", "")})
    result = handler(params.get("args") or {}, **extra)
    if inspect.iscoroutine(result):
        result = asyncio.run(result)
    if isinstance(result, str):
        return result
    return json.dumps(typed.to_wire(result, f"tool {params['name']}"))


def _hook(_, params):
    name = params["hook"]
    kwargs = hooks.payload_for(name, params.get("payload") or {})
    results = []
    for callback in state.REGISTRY["hooks"].get(name, []):
        result = callback(**hooks.narrowed(callback, kwargs))
        if inspect.iscoroutine(result):
            result = asyncio.run(result)
        results.append(_jsonable(result))
    return {"results": results}


def _jsonable(value):
    try:
        return typed.to_wire(value)
    except typed.Unserializable:
        return str(value)


def _command(sidecar, params):
    kind = params.get("kind", "slash")
    name = params["name"]
    if kind == "cli":
        return {"code": cli.run(name, params.get("argv") or [], sidecar.real_stderr)}
    if kind == "section":
        content = state.REGISTRY["sections"][name]["content"]
        return content if isinstance(content, str) else str(content(dict(params.get("info") or {})))
    if kind == "transport":
        request = context.ApprovalRequest.of(params.get("request") or {})
        result = state.REGISTRY["transports"][name](request)
        if inspect.iscoroutine(result):
            result = asyncio.run(result)
        if isinstance(result, str):
            result = request.respond(result)
        return _jsonable(result)
    handler = state.REGISTRY["commands"][name]["handler"]
    result = handler(params.get("args") or "")
    if inspect.iscoroutine(result):
        result = asyncio.run(result)
    return None if result is None else str(result)


def _provider(_, params):
    kind, name, method = params.get("kind", "model"), params["provider"], params["method"]
    args = params.get("args") or {}
    if kind == "model":
        return _model_hook(state.REGISTRY["providers"][name], method, args)
    target = state.REGISTRY["provider_objects"].get(kind, {}).get(name)
    if target is None:
        raise Failure(PLUGIN_ERROR, f"{kind} provider '{name}' is not registered by plugin '{state.PLUGIN_ID}'")
    return typed.call(getattr(target, method), args, f"{name}.{method}")


def _model_hook(profile, method, args):
    context_ = dict(args.get("context") or {})
    if method == "headers":
        headers = dict(profile.default_headers or {})
        headers.update((profile.build_client_kwargs_extras(**context_) or {}).get("default_headers") or {})
        _, top = profile.build_api_kwargs_extras(**context_) or ({}, {})
        headers.update((top or {}).get("extra_headers") or {})
        return {str(k): str(v) for k, v in headers.items()}
    if method == "prepare_request":
        body = dict(args.get("body") or {})
        if isinstance(body.get("messages"), list):
            body["messages"] = profile.prepare_messages(body["messages"])
        body.update(profile.build_extra_body(**context_) or {})
        extra, top = profile.build_api_kwargs_extras(**context_) or ({}, {})
        body.update(extra or {})
        body.update({k: v for k, v in (top or {}).items() if k != "extra_headers"})
        return typed.to_wire(body, f"{profile.name}.prepare_request")
    if method == "list_models":
        return typed.to_wire(profile.fetch_models() or [], f"{profile.name}.fetch_models")
    if method == "classify_error":
        return _jsonable(profile.classify_api_error(**args))
    raise Failure(INVALID_PARAMS, f"unknown provider hook '{method}'")


def _event(_, params):
    name = params.get("event", "")
    for pattern, callback in state.REGISTRY["subscriptions"]:
        if pattern == name or pattern == "*" or (pattern.endswith("*") and name.startswith(pattern[:-1])):
            callback(name, params.get("payload") or {})
    return {}


def _unload(sidecar, _):
    sidecar._unload()
    return {}


HANDLERS = {"plugin.load": _load, "tool.invoke": _tool, "hook.invoke": _hook, "command.invoke": _command,
            "provider.call": _provider, "event.deliver": _event, "plugin.unload": _unload}


def main():
    real_stderr = sys.stderr
    channel_out = os.fdopen(os.dup(1), "wb", buffering=0)
    channel_in = os.fdopen(os.dup(0), "rb", buffering=0)
    # Plugin output must never reach the protocol channel.
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    devnull = os.open(os.devnull, os.O_RDONLY)
    os.dup2(devnull, 0)
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "hermes_api"))
    sidecar = Sidecar(Wire(channel_in, channel_out), real_stderr)
    context.Host.call = sidecar.host_call
    return sidecar.run()
