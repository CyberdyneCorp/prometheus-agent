"""The remote meet nodes this profile approved: ``nodes.json`` in the
plugin's data directory (it holds their tokens)."""

from __future__ import annotations

import time
from typing import Any, Optional

from . import runtime
from .jsonfile import read_json, write_json


class NodeRegistry:
    def __init__(self) -> None:
        self.path = runtime.paths().nodes

    def _load(self) -> dict[str, dict[str, Any]]:
        data = read_json(self.path)
        nodes = data.get("nodes") if isinstance(data, dict) else None
        return nodes if isinstance(nodes, dict) else {}

    def get(self, name: str) -> Optional[dict[str, Any]]:
        entry = self._load().get(name)
        return None if entry is None else {"name": name, **entry}

    def add(self, name: str, url: str, token: str) -> None:
        for label, value in (("node name", name), ("url", url), ("token", token)):
            if not isinstance(value, str) or not value:
                raise ValueError(f"{label} must be a non-empty string")
        if not url.startswith(("ws://", "wss://")):
            raise ValueError("url must be ws:// or wss://")
        nodes = self._load()
        nodes[name] = {"url": url, "token": token, "added_at": time.time()}
        write_json(self.path, {"nodes": nodes})

    def remove(self, name: str) -> bool:
        nodes = self._load()
        if nodes.pop(name, None) is None:
            return False
        write_json(self.path, {"nodes": nodes})
        return True

    def list_all(self) -> list[dict[str, Any]]:
        return [{"name": name, **entry} for name, entry in sorted(self._load().items())]

    def resolve(self, name: Optional[str]) -> Optional[dict[str, Any]]:
        """The named node, or (no name / ``auto``) the only one registered."""
        if name and name != "auto":
            return self.get(name)
        nodes = self.list_all()
        return nodes[0] if len(nodes) == 1 else None
