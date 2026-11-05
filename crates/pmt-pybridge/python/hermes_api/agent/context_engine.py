"""``agent.context_engine``: the ContextEngine base class."""

from abc import ABC, abstractmethod
from typing import Any


class ContextEngine(ABC):
    @property
    @abstractmethod
    def name(self) -> str: ...

    def update_from_response(self, usage: dict) -> None:
        pass

    def should_compress(self, prompt_tokens: int | None = None) -> bool:
        return False

    @abstractmethod
    def compress(self, messages: list, **kwargs: Any) -> list: ...

    def on_session_start(self, session_id: str, **kwargs) -> None:
        pass

    def on_session_end(self, session_id: str, messages: list) -> None:
        pass

    def get_tool_schemas(self) -> list:
        return []

    def handle_tool_call(self, name: str, args: dict, **kwargs) -> str:
        raise NotImplementedError(name)
