"""Bridge test plugin: tools, a narrow hook, a provider object, commands."""

import ctypes
import dataclasses
import json
import os
import subprocess
import sys
import time

from agent.secret_scope import get_secret
from agent.web_search_provider import WebSearchProvider

_CTX = None


def _echo(args, **kwargs):
    return json.dumps({"args": args, "kwargs": sorted(kwargs)})


def _big(args, **_):
    return "x" * (20 * 1024 * 1024)


def _slow(args, **_):
    time.sleep(float(args.get("seconds", 5)))
    return "done"


def _stall(args, **_):
    # A native call that keeps the GIL: nothing else in the process runs.
    ctypes.PyDLL(None).sleep(int(args.get("seconds", 60)))
    return "woke"


def _crash(args, **_):
    os.abort()


def _spawn(args, **_):
    child = subprocess.Popen(["sleep", "600"])
    return str(child.pid)


def _hog(args, **_):
    hoard = []
    while True:
        hoard.append(bytearray(16 * 1024 * 1024))
        time.sleep(0.01)


def _whoami(args, **_):
    return json.dumps({
        "env": sorted(os.environ),
        "openai": os.environ.get("OPENAI_API_KEY"),
        "spotify": os.environ.get("SPOTIFY_TOKEN"),
        "home": os.environ.get("HOME"),
        "cwd": _CTX.session_cwd(),
        "surface": _CTX.session_surface(),
        "profile": _CTX.profile_name,
        "python": sys.executable,
    })


def _secret(args, **_):
    try:
        return get_secret(args["name"]) or ""
    except PermissionError as err:
        return f"PermissionError: {err}"


def _read(args, **_):
    try:
        with open(os.path.expanduser(args["path"]), encoding="utf-8") as handle:
            return handle.read()
    except OSError as err:
        return f"{type(err).__name__}: {err}"


def _inject(args, **_):
    return json.dumps(_CTX.inject_message(args.get("content", "hi"), session_key=args.get("session_key", "s")))


def _fetch(args, **_):
    import urllib.request
    try:
        with urllib.request.urlopen(args["url"], timeout=5) as answer:
            return answer.read().decode()
    except Exception as err:  # noqa: BLE001
        return f"{type(err).__name__}: {err}"


def _emit(args, **_):
    return str(_CTX.emit("pinged", {"n": 1}))


def _on_event(name, payload):
    with open(os.path.join(os.environ["HOME"], "events.json"), "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"name": name, "payload": payload}) + "\n")


def _unsupported(args, **_):
    _CTX.register_telegram_handler(lambda: None)


TOOLS = {
    "echo": _echo, "big": _big, "slow": _slow, "stall": _stall, "crash": _crash, "spawn": _spawn,
    "hog": _hog, "whoami": _whoami, "secret": _secret, "read": _read, "inject": _inject, "fetch": _fetch,
    "unsupported": _unsupported, "emit": _emit,
}


@dataclasses.dataclass
class Result:
    title: str
    url: str
    score: float = 0.5


class Search(WebSearchProvider):
    @property
    def name(self) -> str:
        return "echo-search"

    def is_available(self) -> bool:
        return True

    def search(self, query: str, limit: int = 5) -> list[Result]:
        return [Result(title=f"{query} {n}", url=f"https://example.com/{n}") for n in range(limit)]


def _after_tool(tool_name, result):
    with open(os.path.join(os.environ["HOME"], "hook.json"), "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"tool_name": tool_name, "result": result}) + "\n")


def _cli_setup(parser):
    parser.add_argument("word")
    parser.add_argument("--times", type=int, default=1)


def _cli_run(args):
    print(" ".join([args.word] * args.times))
    line = sys.stdin.readline()
    if line:
        print(f"read {line.strip()}")
    return 3 if args.word == "three" else None


def register(ctx):
    global _CTX
    _CTX = ctx
    dropped = os.path.join(os.environ.get("HOME", ""), "drop_tool")
    drop = open(dropped, encoding="utf-8").read().strip() if os.path.exists(dropped) else ""
    for name, handler in TOOLS.items():
        if name == drop:
            continue
        ctx.register_tool(name=name, toolset="echo", schema={
            "name": name, "description": f"test tool {name}",
            "parameters": {"type": "object", "properties": {}},
        }, handler=handler)
    ctx.register_hook("post_tool_call", _after_tool)
    ctx.register_web_search_provider(Search())
    ctx.register_command("shout", lambda raw: raw.upper(), description="Shout it")
    ctx.register_cli_command("say", "Say a word", _cli_setup, _cli_run)
    ctx.register_system_prompt_section("echo.rules", "Echo plugin rules.")
    ctx.subscribe("other:*", _on_event)
    ctx.on_unload(lambda: open(os.path.join(os.environ["HOME"], "unloaded"), "w").close())
