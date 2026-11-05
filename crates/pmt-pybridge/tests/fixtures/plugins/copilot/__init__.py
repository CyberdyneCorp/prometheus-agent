"""A Hermes model-provider plugin in the shape of the Copilot profile."""

import os

from providers import register_provider
from providers.base import ProviderProfile


class CopilotProfile(ProviderProfile):
    def build_api_kwargs_extras(self, *, reasoning_config=None, supports_reasoning=False, model=None,
                                session_id=None, **context):
        if os.path.exists(os.path.join(os.environ.get("HOME", ""), "crash_headers")):
            os.abort()
        headers = {"Editor-Version": "prometheus/1.0", "Copilot-Integration-Id": "prometheus-tests"}
        return {}, {"extra_headers": headers}


copilot = CopilotProfile(
    name="copilot",
    api_mode="chat_completions",
    env_vars=("COPILOT_API_KEY",),
    base_url=os.environ.get("COPILOT_BASE_URL", "https://api.githubcopilot.com"),
    default_headers={"User-Agent": "prometheus-copilot"},
    fallback_models=("gpt-4.1",),
)

register_provider(copilot)
