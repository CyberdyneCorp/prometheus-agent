"""google_meet: the agent joins a Google Meet call, transcribes its live
captions and, in realtime mode, speaks (spec: plugin-system, First-party
google_meet plugin).

A headless Chromium (Playwright, in the plugin's own browser runtime) joins
the URL in a detached background process, turns on Meet's captions and
scrapes them into ``<profile_home>/workspace/meetings/<id>/transcript.txt``;
no meeting audio is decoded. Realtime mode synthesizes replies (OpenAI
Realtime) and plays them through a virtual microphone. Only
``https://meet.google.com/`` URLs passed in are ever joined: no calendar
scanning, no auto-dial and no consent announcement.
"""

from __future__ import annotations

from . import cli
from . import process_manager as pm
from . import runtime
from .tools import TOOLS


def _on_session_end(session_id: str = "", **_kwargs) -> None:
    """Leaves a call this session joined that is still running, so no
    headless browser is orphaned (a meeting joined from the CLI or another
    session is left alone)."""
    status = pm.status()
    if status.get("ok") and status.get("alive") and session_id and status.get("sessionId") == session_id:
        pm.stop(reason="session ended")


def register(ctx) -> None:
    runtime.CTX = ctx
    for name, schema, handler in TOOLS:
        # Offered only with the browser runtime and the saved sign-in.
        ctx.register_tool(name=name, toolset="google_meet", schema=schema, handler=handler,
                          check_fn=runtime.available)
    for name, (text, setup, handler) in cli.COMMANDS.items():
        ctx.register_cli_command(name=name, help=text, setup_fn=setup, handler_fn=handler,
                                 description=text)
    ctx.register_hook("on_session_end", _on_session_end)
