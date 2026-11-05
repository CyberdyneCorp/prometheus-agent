"""``agent.transcription_provider``."""

from typing import Any

from agent.provider_base import CatalogProviderBase


class TranscriptionProvider(CatalogProviderBase):
    def transcribe(self, path: str, **kwargs: Any) -> Any:
        raise NotImplementedError(f"{self.name} does not implement transcribe")
