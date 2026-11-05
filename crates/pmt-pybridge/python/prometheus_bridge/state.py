"""Process-wide state of the sidecar: what the plugin registered, what it
was granted and the call it is serving now."""

import threading

# Set by plugin.load.
PLUGIN_ID = ""
PROFILE = ""
GRANTED_SECRETS = frozenset()
CAPABILITIES = frozenset()
DATA_DIR = ""
API = {}

# What the plugin registered.
REGISTRY = {
    "tools": {},
    "hooks": {},
    "middleware": {},
    "commands": {},
    "cli_commands": {},
    "sections": {},
    "skills": {},
    "providers": {},
    "provider_objects": {},
    "memory_providers": {},
    "context_engines": {},
    "context_references": [],
    "transports": {},
    "subscriptions": [],
    "auxiliary_tasks": {},
    "redaction_patterns": [],
    "locales": [],
    "blueprints": [],
    "unload": [],
    "degraded": [],
}

_local = threading.local()


def current_call():
    """The context of the call this thread serves (session, surface, cwd)."""
    return getattr(_local, "call", None) or {}


def set_call(context):
    _local.call = context or {}
