"""``agent.secret_scope``: secrets a bridged plugin was granted."""

import os

from prometheus_bridge import state


def get_secret(name, default=None):
    """The value of granted secret ``name``; PermissionError for any other."""
    if name not in state.GRANTED_SECRETS:
        raise PermissionError(
            f"secret '{name}' is not granted to plugin '{state.PLUGIN_ID}' "
            "(add it to its secrets capability)"
        )
    return os.environ.get(name, default)


def get_secret_str(name, default=""):
    value = get_secret(name, default)
    return "" if value is None else str(value)
