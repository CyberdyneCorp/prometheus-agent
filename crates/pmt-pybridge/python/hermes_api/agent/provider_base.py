"""``agent.provider_base``: identity shared by every provider object."""

import abc
from typing import Any, Optional


class ProviderBase(abc.ABC):
    @property
    @abc.abstractmethod
    def name(self) -> str: ...

    @property
    def display_name(self) -> str:
        return self.name

    def get_setup_schema(self) -> dict:
        return {"name": self.display_name, "badge": "", "tag": "", "env_vars": []}


class CatalogProviderBase(ProviderBase):
    @property
    def display_name(self) -> str:
        return self.name.title()

    def is_available(self) -> bool:
        return True

    def list_models(self) -> list:
        return []

    def default_model(self) -> Optional[str]:
        models = self.list_models()
        return models[0].get("id") if models else None


def _unused(_: Any) -> None:
    return None
