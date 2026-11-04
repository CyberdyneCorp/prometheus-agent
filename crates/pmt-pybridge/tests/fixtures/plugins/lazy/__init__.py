def _ping(args, **_):
    return "pong"


def register(ctx):
    ctx.register_tool(name="ping", toolset="lazy", schema={"name": "ping", "parameters": {"type": "object"}},
                      handler=_ping)
