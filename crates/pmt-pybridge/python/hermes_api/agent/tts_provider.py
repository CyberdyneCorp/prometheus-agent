"""``agent.tts_provider``."""

from typing import Any

from agent.provider_base import CatalogProviderBase


class TTSProvider(CatalogProviderBase):
    def synthesize(self, text: str, **kwargs: Any) -> Any:
        raise NotImplementedError(f"{self.name} does not implement synthesize")
