"""A Hermes tool plugin, written against the Hermes plugin API only."""

import json

from agent.secret_scope import get_secret

SCHEMA = {
    "name": "spotify_play",
    "description": "Play a track on Spotify.",
    "parameters": {
        "type": "object",
        "properties": {"track": {"type": "string"}},
        "required": ["track"],
    },
}


def _play(args, **kwargs):
    token = get_secret("SPOTIFY_TOKEN") or ""
    return json.dumps({"playing": args.get("track"), "authorized": bool(token)})


def register(ctx):
    ctx.register_tool(name="spotify_play", toolset="spotify", schema=SCHEMA, handler=_play)
