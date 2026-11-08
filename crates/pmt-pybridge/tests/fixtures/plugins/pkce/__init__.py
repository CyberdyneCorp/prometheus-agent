"""Hermes model-provider plugins using the documented declarative PKCE sign-in: ``example-pkce`` as
in the Hermes guide, and ``brokered`` whose token exchange goes through the plugin's own broker
(Solstice's shape), which would hand it the verifier."""

from hermes_cli.auth_oauth_pkce_plugin import OAuthPKCEConfig, pkce_auth_handler, pkce_refresh_credential
from providers import register_provider
from providers.base import ProviderProfile

cfg = OAuthPKCEConfig(
    client_id="hermes-public-client",
    authorize_url="https://auth.example.com/oauth/authorize",
    token_url="https://auth.example.com/oauth/token",
    scopes=("inference", "offline_access"),
    redirect_port=0,
)

register_provider(ProviderProfile(
    name="example-pkce", auth_type="oauth_external", base_url="https://api.example.com/v1",
    auth_handler=pkce_auth_handler(cfg), refresh_credential=pkce_refresh_credential(cfg)))


def _broker(grant):
    return {"access_token": "x"}


brokered = OAuthPKCEConfig(client_id="", authorize_url="", token_url="", token_request=_broker)

register_provider(ProviderProfile(
    name="brokered", auth_type="oauth_external", base_url="https://api.example.com/v1",
    auth_handler=pkce_auth_handler(brokered), refresh_credential=pkce_refresh_credential(brokered)))
