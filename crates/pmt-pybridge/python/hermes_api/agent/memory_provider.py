"""``agent.memory_provider``: the MemoryProvider base class."""

import contextvars
import threading
from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, Callable, Optional


def ctx_bound(fn: Callable[..., Any]) -> Callable[..., Any]:
    ctx = contextvars.copy_context()
    return lambda *args, **kwargs: ctx.run(fn, *args, **kwargs)


def spawn_context_thread(target: Callable[..., Any], *, name: str, daemon: bool = True,
                         args: tuple = (), kwargs: Optional[dict] = None) -> threading.Thread:
    return threading.Thread(target=ctx_bound(target), args=args, kwargs=kwargs, name=name, daemon=daemon)


@dataclass(frozen=True)
class RecallStatus:
    provider_label: str
    count: int
    glyph: str = "\U0001f9e0"


class MemoryProvider(ABC):
    @property
    @abstractmethod
    def name(self) -> str: ...

    def is_available(self) -> bool:
        return True

    def initialize(self, session_id: str, **kwargs) -> None:
        pass

    def unavailable_reason(self) -> str:
        return ""

    def system_prompt_block(self) -> str:
        return ""

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        return ""

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        pass

    def recall_status(self) -> Optional[RecallStatus]:
        return None

    def sync_turn(self, user_content: str, assistant_content: str, *, session_id: str = "", **kwargs) -> None:
        pass

    def get_tool_schemas(self) -> list:
        return []

    def handle_tool_call(self, tool_name: str, args: dict, **kwargs) -> str:
        raise NotImplementedError(tool_name)

    def shutdown(self) -> None:
        pass

    def on_turn_start(self, turn_number: int, message: str, **kwargs) -> None:
        pass

    def on_session_end(self, messages: list) -> None:
        pass

    def on_pre_compress(self, messages: list) -> str:
        return ""

    def on_memory_write(self, action: str, target: str, content: str, metadata: Optional[dict] = None) -> None:
        pass
