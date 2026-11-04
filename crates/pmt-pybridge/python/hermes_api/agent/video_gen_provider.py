"""``agent.video_gen_provider``."""

from typing import Any

from agent.provider_base import CatalogProviderBase


class VideoGenProvider(CatalogProviderBase):
    def generate(self, prompt: str, **kwargs: Any) -> Any:
        raise NotImplementedError(f"{self.name} does not implement generate")
