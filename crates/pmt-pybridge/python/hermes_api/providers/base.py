"""``providers.base``: the ProviderProfile a model-provider plugin registers."""

from dataclasses import dataclass, field
from typing import Any, Callable


@dataclass
class ProviderProfile:
    name: str
    api_mode: str = "chat_completions"
    aliases: tuple = ()
    display_name: str = ""
    description: str = ""
    signup_url: str = ""
    hidden: bool = False
    env_vars: tuple = ()
    base_url: str = ""
    models_url: str = ""
    auth_type: str = "api_key"
    supports_model_listing: bool = True
    fallback_models: tuple = ()
    default_headers: dict = field(default_factory=dict)
    default_aux_model: str = ""
    classify_api_error: Callable[..., Any] | None = None

    def prepare_messages(self, messages: list) -> list:
        return messages

    def build_extra_body(self, *, session_id: str | None = None, **context: Any) -> dict:
        return {}

    def build_api_kwargs_extras(self, *, reasoning_config: dict | None = None, supports_reasoning: bool = False,
                                model: str | None = None, session_id: str | None = None, **context: Any):
        return {}, {}

    def build_client_kwargs_extras(self, **context: Any) -> dict:
        return {}

    def fetch_models(self, *, api_key: str | None = None, base_url: str | None = None, timeout: float = 8.0):
        return list(self.fallback_models) or None
