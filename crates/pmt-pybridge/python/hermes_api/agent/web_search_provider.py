"""``agent.web_search_provider``."""

import abc
from typing import Any

from agent.provider_base import ProviderBase


class WebSearchProvider(ProviderBase):
    @abc.abstractmethod
    def is_available(self) -> bool: ...

    def supports_search(self) -> bool:
        return True

    def supports_extract(self) -> bool:
        return False

    def search(self, query: str, limit: int = 5) -> Any:
        raise NotImplementedError(f"{self.name} does not support search")

    def extract(self, urls: list, **kwargs: Any) -> Any:
        raise NotImplementedError(f"{self.name} does not support extract")
