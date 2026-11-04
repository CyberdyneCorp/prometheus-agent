"""A Hermes memory-provider plugin whose CLI lives in cli.py (``hermes memcli ...``)."""

from agent.memory_provider import MemoryProvider


class MemcliProvider(MemoryProvider):
    @property
    def name(self):
        return "memcli"

    def get_tool_schemas(self):
        return []


def register(ctx):
    ctx.register_memory_provider(MemcliProvider())
