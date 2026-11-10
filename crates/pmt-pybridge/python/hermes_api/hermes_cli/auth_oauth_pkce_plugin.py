"""``hermes_cli.auth_oauth_pkce_plugin``: the documented declarative OAuth PKCE sign-in.

Mirrors Hermes ``hermes_cli/auth_oauth_pkce_plugin.py`` (hermes-agent, MIT) at the plugin-facing surface:
``OAuthPKCEConfig`` and the two factories. Under Prometheus the host runs the whole flow (spec:
model-providers, Plugin OAuth and external-process providers): the bridge reads the config the factories
carry into the provider descriptor's ``oauth`` block, ``prometheus auth add <name> --type oauth`` runs the
browser PKCE leg, the token exchange and refreshes, and the plugin never sees the verifier, ``state`` or a
token. The handlers themselves therefore only point at the host command when called.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable, Mapping, Optional

POOL_SOURCE = "manual:loopback_pkce"

# The attribute the bridge reads the config from (``prometheus_bridge.sidecar._oauth``).
CONFIG_ATTR = "__prometheus_oauth__"


@dataclass(frozen=True)
class OAuthPKCEConfig:
    """Public-client OAuth metadata for one provider. No client secret: PKCE is the proof."""

    client_id: str
    authorize_url: str
    token_url: str
    scopes: tuple[str, ...] = ()
    redirect_port: int = 0
    redirect_path: str = "/callback"
    audience: Optional[str] = None
    extra_authorize_params: Mapping[str, str] = field(default_factory=dict)
    extra_token_params: Mapping[str, str] = field(default_factory=dict)
    allowed_hosts: tuple[str, ...] = ()
    timeout_seconds: float = 180.0
    label: str = ""
    # A broker that receives the grant (verifier included) is refused at load: the host runs the
    # token exchange so no plugin ever holds the verifier.
    token_request: Optional[Callable[[dict[str, str]], Mapping[str, Any]]] = None


def _host_runs_it(action, *_args, **_kwargs):
    raise RuntimeError(
        f"the Prometheus host runs this sign-in ({action!r}): use `prometheus auth add <provider> --type oauth`"
    )


def pkce_auth_handler(cfg: OAuthPKCEConfig) -> Callable[[str, Any], bool]:
    """The ``ProviderProfile.auth_handler`` of ``cfg``; the bridge turns it into the host's sign-in."""

    def handler(action: str, args: Any = None) -> bool:
        return _host_runs_it(action)

    setattr(handler, CONFIG_ATTR, cfg)
    return handler


def pkce_refresh_credential(cfg: OAuthPKCEConfig) -> Callable[[Any], Optional[Mapping[str, Any]]]:
    """The ``ProviderProfile.refresh_credential`` of ``cfg``; refreshes run in the host."""

    def refresh(entry: Any) -> Optional[Mapping[str, Any]]:
        return _host_runs_it("refresh")

    setattr(refresh, CONFIG_ATTR, cfg)
    return refresh
