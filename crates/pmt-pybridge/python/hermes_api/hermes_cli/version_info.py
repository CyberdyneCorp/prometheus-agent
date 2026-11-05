"""``hermes_cli.version_info``: the User-Agent helper the model-provider guide documents
for ``LazyHeaders`` values (``hermes_user_agent("MyAgent")``)."""

HERMES_COMPAT_VERSION = "2026.10"


def hermes_user_agent(product: str = "hermes-cli") -> str:
    """``<product>/<version>``: the Hermes release whose plugin API the bridge implements."""
    return f"{product}/{HERMES_COMPAT_VERSION}"
