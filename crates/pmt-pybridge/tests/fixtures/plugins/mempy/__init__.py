"""A Hermes memory provider: recalls ``py-recalled: <query>`` and keeps
each synced turn."""

from agent.memory_provider import MemoryProvider


class MempyProvider(MemoryProvider):
    def __init__(self):
        self.turns = []

    @property
    def name(self):
        return "mempy"

    def system_prompt_block(self):
        return "MEMPY BLOCK"

    def prefetch(self, query, *, session_id=""):
        return f"py-recalled: {query}"

    def sync_turn(self, user_content, assistant_content, *, session_id="", **kwargs):
        self.turns.append(user_content)


def register(ctx):
    ctx.register_memory_provider(MempyProvider())
