"""``agent.secret_sources.base``: the SecretSource base class."""

from abc import ABC, abstractmethod
from typing import Optional


class SecretSource(ABC):
    @property
    @abstractmethod
    def name(self) -> str: ...

    @abstractmethod
    def get(self, key: str) -> Optional[str]: ...
