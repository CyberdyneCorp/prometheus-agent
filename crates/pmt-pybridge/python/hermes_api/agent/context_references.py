"""``agent.context_references``: the ContextReferenceProvider base class."""

from abc import ABC, abstractmethod


class ContextReferenceProvider(ABC):
    @property
    @abstractmethod
    def name(self) -> str: ...
