"""The PluginContext a plugin's ``register(ctx)`` receives: the supported
subset of the Hermes API, mapped onto the Prometheus host through
``host.call`` requests. Anything else raises UnsupportedPluginApi."""

import asyncio
import dataclasses
import inspect
import json
import os
import re
import threading

from . import hooks as hook_payloads
from . import state

_TRANSPORT_NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}")
_SECTION_ID = re.compile(r"[a-z0-9._-]{1,128}")
_SYNC_WAIT = 30.0


class UnsupportedPluginApi(Exception):
    """A Hermes API the bridge does not implement."""


def _unsupported(name):
    api = state.API
    reason = api.get("unsupported", {}).get(name) or api.get("other_reason", "")
    return UnsupportedPluginApi(f"ctx.{name}() is not supported by the Prometheus python bridge: {reason}")


class Host:
    """Requests to the host; set by the sidecar."""

    call = None  # function(method, args, timeout) -> result (raises HostError)


class HostError(Exception):
    """The host refused or failed a request."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def host(method, args=None, timeout=None):
    return Host.call(method, args or {}, timeout)


class PostedMessage:
    """A live message handle: ``edit``/``aedit``, ``ok``, ``message_id``, ``error``, ``pending``."""

    def __init__(self, session_id):
        self._session = session_id
        self.ok = False
        self.message_id = None
        self.error = None
        self.pending = False
        self._done = threading.Event()

    def _settle(self, result=None, error=None):
        if error is not None:
            self.ok, self.error = False, error
        else:
            self.ok = True
            self.message_id = (result or {}).get("handle")
            self.error = None
        self.pending = False
        self._done.set()

    def _send(self, method, args):
        def run():
            try:
                self._settle(host(method, args, None))
            except HostError as err:
                self._settle(error=err.code or str(err))
        self.pending = True
        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        if not self._done.wait(_SYNC_WAIT):
            self.error = "timeout"
        return self

    def edit(self, text):
        if not self.message_id:
            self.ok, self.error = False, self.error or "no_message"
            return self
        self._done.clear()
        return self._send("messaging.edit", {"session_id": self._session, "handle": self.message_id, "text": text})

    async def aedit(self, text):
        return await asyncio.to_thread(self.edit, text)


class PlatformActions:
    """``ctx.platform_actions``: verbs answer ``{"ok": bool, ...}`` and never raise."""

    def _run(self, verb, **args):
        try:
            return host("messaging.platform_action", {"verb": verb, **args}, _SYNC_WAIT)
        except HostError as err:
            return {"ok": False, "error": err.code or "action_failed", "detail": str(err)}

    def add_reaction(self, platform, chat_id, message_id, emoji):
        return self._run("add_reaction", platform=platform, chat_id=str(chat_id), target_id=str(message_id), value=emoji)

    def set_thread_title(self, platform, chat_id, thread_id, title):
        return self._run("set_thread_title", platform=platform, chat_id=str(chat_id), target_id=str(thread_id), value=title)


@dataclasses.dataclass(frozen=True)
class ApprovalDecision:
    """A transport's answer bound to one host request (Hermes shape)."""

    request_id: str
    request_digest: str
    choice: str


@dataclasses.dataclass(frozen=True)
class ApprovalRequest:
    """The immutable, redacted request an approval transport presents."""

    schema_version: int
    request_id: str
    digest: str
    command: str
    description: str
    pattern_key: str
    pattern_keys: tuple
    surface: str
    timeout_seconds: float
    allowed_choices: tuple

    @classmethod
    def of(cls, data):
        return cls(schema_version=1, request_id=data.get("request_id", ""), digest=data.get("digest", ""),
                   command=data.get("command", ""), description=data.get("description", ""), pattern_key="",
                   pattern_keys=(), surface=data.get("surface", ""),
                   timeout_seconds=float(data.get("timeout_seconds") or 0),
                   allowed_choices=tuple(data.get("allowed_choices") or ()))

    def respond(self, choice):
        return ApprovalDecision(self.request_id, self.digest, str(choice))


class PluginLlm:
    """``ctx.llm``: completions on the profile's model through the host."""

    @staticmethod
    def _messages(prompt, messages, system):
        out = []
        if system:
            out.append({"role": "system", "content": system})
        if messages:
            out.extend({"role": m.get("role", "user"), "content": str(m.get("content", ""))} for m in messages)
        if prompt:
            out.append({"role": "user", "content": prompt})
        return out

    def complete(self, prompt=None, *, messages=None, system=None, model=None, provider=None, max_tokens=None, **_):
        return host("llm.complete", {"messages": self._messages(prompt, messages, system), "model": model,
                                     "provider": provider, "max_tokens": max_tokens})

    def complete_structured(self, prompt=None, *, schema, messages=None, system=None, model=None, provider=None,
                            max_tokens=None, **_):
        text = host("llm.complete", {"messages": self._messages(prompt, messages, system), "model": model,
                                     "provider": provider, "max_tokens": max_tokens, "schema": json.dumps(schema)})
        return json.loads(text)

    async def acomplete(self, *args, **kwargs):
        return await asyncio.to_thread(self.complete, *args, **kwargs)

    async def acomplete_structured(self, *args, **kwargs):
        return await asyncio.to_thread(self.complete_structured, *args, **kwargs)


class PluginState:
    """``ctx.state``: durable JSON values in the plugin's own data directory."""

    def __init__(self):
        self._lock = threading.Lock()

    def _path(self):
        return os.path.join(state.DATA_DIR, "state.json")

    def _load(self):
        try:
            with open(self._path(), encoding="utf-8") as handle:
                return json.load(handle)
        except (OSError, ValueError):
            return {}

    def _save(self, data):
        os.makedirs(state.DATA_DIR, exist_ok=True)
        tmp = self._path() + ".part"
        with open(tmp, "w", encoding="utf-8") as handle:
            json.dump(data, handle)
        os.replace(tmp, self._path())

    def get(self, key, default=None):
        with self._lock:
            return self._load().get(key, default)

    def set(self, key, value):
        with self._lock:
            data = self._load()
            data[key] = value
            self._save(data)

    def delete(self, key):
        with self._lock:
            data = self._load()
            data.pop(key, None)
            self._save(data)

    def keys(self):
        with self._lock:
            return list(self._load())


class PluginContext:
    """The supported Hermes PluginContext subset."""

    def __init__(self, manifest):
        self.manifest = manifest
        self._state = PluginState()
        self._platform_actions = PlatformActions()
        self._llm = PluginLlm()

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        raise _unsupported(name)

    # Identity and session facts.
    @property
    def plugin_id(self):
        return state.PLUGIN_ID

    @property
    def profile_name(self):
        return state.PROFILE

    @property
    def state(self):
        return self._state

    @property
    def platform_actions(self):
        return self._platform_actions

    @property
    def llm(self):
        return self._llm

    def has_plugin(self, plugin_id):
        return bool(host("plugins.has", {"id": plugin_id}))

    def has_capability(self, capability):
        return capability in state.CAPABILITIES

    def session_cwd(self):
        return state.current_call().get("cwd") or ""

    def session_surface(self):
        return state.current_call().get("surface") or ""

    def get_config(self, key, default=None):
        value = host("config.get", {"key": key})
        return default if value is None else value

    def set_config(self, key, value):
        host("config.set", {"key": key, "value": value})

    # Registrations.
    def register_tool(self, name, toolset, schema, handler, check_fn=None, requires_env=None, is_async=False,
                      description="", emoji="", override=False):
        state.REGISTRY["tools"][name] = {
            "name": name, "toolset": toolset, "schema": schema, "handler": handler,
            "description": description or (schema or {}).get("description", ""), "override": bool(override),
            "check_fn": check_fn,
        }

    def register_hook(self, hook_name, callback, *, hold_stream=False):
        if hook_name not in state.API.get("hooks", ()):
            raise ValueError(f"unknown hook '{hook_name}'")
        if hook_payloads.wants_placeholder(hook_name, callback):
            state.REGISTRY["degraded"].append(
                f"{hook_name} receives placeholders for the gateway runner and session store")
        state.REGISTRY["hooks"].setdefault(hook_name, []).append(callback)

    def register_middleware(self, kind, callback):
        state.REGISTRY["middleware"].setdefault(kind, []).append(callback)
        state.REGISTRY["degraded"].append(f"middleware '{kind}' is not invoked by Prometheus")

    def register_command(self, name, handler, description="", args_hint="", argument_mode=None):
        clean = str(name).lower().strip().lstrip("/").replace(" ", "-")
        state.REGISTRY["commands"][clean] = {"handler": handler, "description": description or "Plugin command",
                                             "args_hint": args_hint}

    def register_cli_command(self, name, help, setup_fn, handler_fn=None, description=""):
        state.REGISTRY["cli_commands"][name] = {"help": help, "setup_fn": setup_fn, "handler_fn": handler_fn,
                                                "description": description}

    def register_skill(self, name, path, description="", frontmatter=None):
        state.REGISTRY["skills"][name] = {"path": str(path), "description": description}

    def register_system_prompt_section(self, id, content, *, position="after_memory", max_chars=4000):
        if not _SECTION_ID.fullmatch(str(id)):
            raise ValueError("system prompt section id must be 1-128 lowercase characters "
                             "using letters, numbers, '.', '_', or '-'")
        if not isinstance(content, str) and not callable(content):
            raise TypeError("system prompt section content must be a string or callable")
        state.REGISTRY["sections"][id] = {"content": content, "max_chars": int(max_chars)}

    def register_memory_provider(self, provider):
        state.REGISTRY["memory_providers"][provider.name] = provider

    def register_context_engine(self, engine):
        state.REGISTRY["context_engines"][engine.name] = engine

    def register_context_reference(self, provider):
        state.REGISTRY["context_references"].append(provider)

    def _object(self, kind, provider):
        state.REGISTRY["provider_objects"].setdefault(kind, {})[provider.name] = provider

    def register_image_gen_provider(self, provider):
        self._object("image_gen", provider)

    def register_video_gen_provider(self, provider):
        self._object("video_gen", provider)

    def register_web_search_provider(self, provider):
        self._object("web_search", provider)

    def register_tts_provider(self, provider):
        self._object("tts", provider)

    def register_transcription_provider(self, provider):
        self._object("stt", provider)

    def register_secret_source(self, source):
        self._object("secret_source", source)

    def register_auxiliary_task(self, key, *, display_name, description, defaults=None, inherit_from=None):
        state.REGISTRY["auxiliary_tasks"][key] = {"display_name": display_name, "description": description,
                                                  "defaults": defaults or {}, "inherit_from": inherit_from}

    def register_redaction_patterns(self, patterns):
        items = list(patterns or [])
        state.REGISTRY["redaction_patterns"].extend(str(p) for p in items)
        return len(items)

    def register_locale(self, lang, source, *, endonym=None, rtl=False, surface="core"):
        state.REGISTRY["locales"].append({"lang": lang, "source": str(source) if not isinstance(source, dict) else source,
                                          "endonym": endonym, "rtl": rtl, "surface": surface})

    def register_locale_dir(self, path, *, surface="core"):
        state.REGISTRY["locales"].append({"dir": str(path), "surface": surface})

    def register_automation_blueprint(self, key, **fields):
        state.REGISTRY["blueprints"].append({"key": key, **{k: v if not isinstance(v, (set, tuple)) else list(v)
                                                              for k, v in fields.items()}})

    def register_provider(self, profile):
        state.REGISTRY["providers"][profile.name] = profile

    def register_approval_transport(self, name, present_fn):
        clean = str(name).strip().lower()
        if clean == "builtin":
            raise ValueError("approval transport name 'builtin' is reserved")
        if not _TRANSPORT_NAME.fullmatch(clean):
            raise ValueError("approval transport name must match [a-z0-9][a-z0-9_-]{0,63}")
        state.REGISTRY["transports"][clean] = present_fn

    # Messages and events.
    def post_message(self, text):
        session = state.current_call().get("session_id") or ""
        return PostedMessage(session)._send("messaging.post", {"session_id": session, "text": str(text)})

    async def apost_message(self, text):
        return await asyncio.to_thread(self.post_message, text)

    def inject_message(self, content, role="user", *, session_key=None, origin=None):
        try:
            return bool(host("messaging.inject", {"content": str(content), "role": role, "session_key": session_key,
                                                  "origin": origin}, _SYNC_WAIT))
        except HostError:
            return False

    def emit(self, event, payload=None):
        if not event or not isinstance(event, str) or ":" in event:
            raise ValueError(f"Plugin '{state.PLUGIN_ID}' may not emit '{event}': emit only the bare event name")
        if payload is not None and not isinstance(payload, dict):
            raise TypeError("emit() payload must be a dict or None")
        return int(host("events.emit", {"name": event, "payload": payload or {}}) or 0)

    def subscribe(self, event, callback):
        state.REGISTRY["subscriptions"].append((event, callback))

    def dispatch_tool(self, tool_name, args, **kwargs):
        return host("tools.dispatch", {"name": tool_name, "arguments": args})

    # Lifecycle.
    def on_unload(self, callback):
        if not callable(callback):
            raise TypeError("on_unload callback must be callable")
        state.REGISTRY["unload"].append(callback)

    def spawn_task(self, coro, *, name=None):
        if not inspect.iscoroutine(coro):
            raise TypeError("spawn_task expects a coroutine")
        thread = threading.Thread(target=asyncio.run, args=(coro,), name=name or f"plugin:{state.PLUGIN_ID}:task",
                                  daemon=True)
        thread.start()
        return thread
