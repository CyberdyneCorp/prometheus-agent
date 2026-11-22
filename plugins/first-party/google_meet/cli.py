"""``prometheus plugins run google_meet <command>``: setup, install, auth,
join, status, transcript, say, stop and node."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from typing import Any, Callable, Optional

from . import process_manager as pm
from . import runtime
from .meeturl import REFUSAL, is_meet_url
from .node_client import NodeClient
from .node_registry import NodeRegistry
from .node_server import NodeServer


def _print(result: dict[str, Any]) -> int:
    print(json.dumps(result, indent=2))
    return 0 if result.get("ok") else 1


def _remote(node: str, call: Callable[[NodeClient], dict]) -> int:
    entry = NodeRegistry().resolve(node)
    if entry is None:
        print(f"no approved node matches {node!r}")
        return 1
    try:
        result = call(NodeClient(entry["url"], entry["token"]))
    except Exception as exc:  # noqa: BLE001 - reported to the user
        print(f"remote node failed: {exc}")
        return 1
    return _print({"node": entry["name"], **result})


# setup

def setup(args: argparse.Namespace) -> int:
    p = runtime.paths()
    system_ok = runtime.supported()
    ready = runtime.runtime_ready(p)
    print("google_meet preflight")
    print(f"  platform       : {platform.system()}  [{'ok' if system_ok else 'unsupported (Linux and macOS only)'}]")
    print("  browser runtime: " + (f"ok ({p.runtime})" if ready else
                                    "not installed: run `prometheus plugins run google_meet install`"))
    print("  google sign-in : " + (f"ok ({p.auth})" if p.auth.is_file() else
                                    "not saved: run `prometheus plugins run google_meet auth`"))
    ok = system_ok and ready and p.auth.is_file()
    print("ready: the meet_* tools are offered in new sessions" if ok else "not ready yet: fix the items above")
    return 0 if ok else 1


# install

def _run(command: list[str], **kwargs) -> bool:
    print("  $ " + " ".join(command))
    return subprocess.run(command, check=False, stdin=subprocess.DEVNULL, **kwargs).returncode == 0


def _audio_hint() -> None:
    if platform.system() == "Linux":
        missing = [t for t in ("pactl", "paplay") if not shutil.which(t)]
        print("  pulseaudio tools: " + ("present" if not missing else
                                         "missing: install pulseaudio-utils (e.g. `sudo apt-get install pulseaudio-utils`)"))
    else:
        print("  install the BlackHole virtual microphone and ffmpeg (e.g. `brew install blackhole-2ch ffmpeg`),")
        print("  then pick 'BlackHole 2ch' as the input in System Settings > Sound before a realtime meeting")


def install(args: argparse.Namespace) -> int:
    if not runtime.supported():
        print(f"google_meet: {platform.system()} is not supported (Linux and macOS only)")
        return 1
    p = runtime.paths()
    print("[1/3] browser runtime environment")
    if not p.python.exists() and not _run([sys.executable, "-m", "venv", str(p.runtime)]):
        return 1
    print("[2/3] Playwright, websockets and Chromium")
    env = {**os.environ, "PLAYWRIGHT_BROWSERS_PATH": str(p.browsers)}
    if not _run([str(p.python), "-m", "pip", "install", "--disable-pip-version-check",
                 runtime.PLAYWRIGHT, runtime.WEBSOCKETS], env=env):
        return 1
    if not _run([str(p.python), "-m", "playwright", "install", "chromium"], env=env):
        return 1
    p.ready.write_text(json.dumps({"playwright": runtime.PLAYWRIGHT, "websockets": runtime.WEBSOCKETS,
                                   "installed_at": time.time()}), encoding="utf-8")
    print("[3/3] realtime audio (mode='realtime' only)")
    _audio_hint()
    print("done: sign in with `prometheus plugins run google_meet auth`")
    return 0


# auth

def auth(args: argparse.Namespace) -> int:
    p = runtime.paths()
    if not runtime.runtime_ready(p):
        print("the browser runtime is not installed: run `prometheus plugins run google_meet install`")
        return 1
    trigger = p.data / "auth.save"
    trigger.unlink(missing_ok=True)
    env = {**os.environ, "PLAYWRIGHT_BROWSERS_PATH": str(p.browsers)}
    child = subprocess.Popen([str(p.python), str(pm.BOT_SCRIPT), "--auth", str(p.auth), str(trigger)],
                             stdin=subprocess.DEVNULL, env=env)
    print("a browser window opens: sign in to Google, then come back here and press Enter")
    try:
        input("press Enter after you have signed in ... ")
    except EOFError:
        pass
    trigger.write_text("save", encoding="utf-8")
    try:
        code = child.wait(timeout=60)
    except subprocess.TimeoutExpired:
        child.kill()
        code = 1
    if code != 0 or not p.auth.is_file():
        print("sign-in state was not saved")
        return 1
    os.chmod(p.auth, 0o600)
    print(f"saved {p.auth}: the meet_* tools are offered in new sessions")
    return 0


# join / status / transcript / say / stop

def join(args: argparse.Namespace) -> int:
    if not is_meet_url(args.url):
        print(REFUSAL)
        return 2
    common = {"url": args.url, "guest_name": args.guest_name, "duration": args.duration,
              "headed": args.headed, "mode": args.mode}
    if args.node:
        return _remote(args.node, lambda c: c.start_bot(**common))
    return _print(pm.start(**common))


def status(args: argparse.Namespace) -> int:
    if args.node:
        return _remote(args.node, lambda c: c.status())
    return _print(pm.status())


def transcript(args: argparse.Namespace) -> int:
    if args.node:
        return _remote(args.node, lambda c: c.transcript(last=args.last))
    result = pm.transcript(last=args.last)
    if not result.get("ok"):
        return _print(result)
    for line in result["lines"]:
        print(line)
    return 0


def say(args: argparse.Namespace) -> int:
    if not args.text.strip():
        print("refusing: empty text")
        return 2
    if args.node:
        return _remote(args.node, lambda c: c.say(args.text))
    return _print(pm.enqueue_say(args.text))


def stop(args: argparse.Namespace) -> int:
    if args.node:
        return _remote(args.node, lambda c: c.stop())
    return _print(pm.stop(reason="prometheus plugins run google_meet stop"))


# node

def _node_run(args: argparse.Namespace) -> int:
    server = NodeServer(args.host, args.port, args.display_name)
    token = server.token()
    print(f"[meet-node] {server.display_name} listening on ws://{args.host}:{args.port}")
    print(f"[meet-node] approve it on the profile that uses it:\n"
          f"  prometheus plugins run google_meet node approve <name> ws://<this host>:{args.port} {token}")
    try:
        asyncio.run(server.serve())
    except KeyboardInterrupt:
        pass
    except (RuntimeError, OSError) as exc:
        print(f"[meet-node] error: {exc}", file=sys.stderr)
        return 2
    return 0


def _node_list(args: argparse.Namespace) -> int:
    nodes = NodeRegistry().list_all()
    if not nodes:
        print("no nodes approved")
    for node in nodes:
        print(f"{node['name']}\t{node['url']}\ttoken={node['token'][:6]}...")
    return 0


def _node_approve(args: argparse.Namespace) -> int:
    try:
        NodeRegistry().add(args.name, args.url, args.token)
    except ValueError as exc:
        print(f"refusing: {exc}")
        return 2
    print(f"approved node {args.name!r} at {args.url}")
    return 0


def _node_remove(args: argparse.Namespace) -> int:
    removed = NodeRegistry().remove(args.name)
    print(f"removed {args.name!r}" if removed else f"no such node: {args.name!r}")
    return 0 if removed else 1


def _node_ping(args: argparse.Namespace) -> int:
    entry = NodeRegistry().get(args.name)
    if entry is None:
        print(f"no such node: {args.name!r}")
        return 1
    try:
        result = NodeClient(entry["url"], entry["token"]).ping()
    except Exception as exc:  # noqa: BLE001 - reported to the user
        print(json.dumps({"ok": False, "error": str(exc)}))
        return 1
    print(json.dumps({"ok": True, "node": args.name, **result}))
    return 0


def _node_parser(parser: argparse.ArgumentParser) -> None:
    sub = parser.add_subparsers(dest="node_command", required=True)
    run = sub.add_parser("run", help="Serve meetings for another machine's profile")
    run.add_argument("--host", default="127.0.0.1")
    run.add_argument("--port", type=int, default=18789)
    run.add_argument("--display-name", default="prometheus-meet-node")
    run.set_defaults(func=_node_run)
    sub.add_parser("list", help="List approved nodes").set_defaults(func=_node_list)
    approve = sub.add_parser("approve", help="Approve a remote node")
    for name in ("name", "url", "token"):
        approve.add_argument(name)
    approve.set_defaults(func=_node_approve)
    for name, handler, text in (("remove", _node_remove, "Forget a node"),
                                ("status", _node_ping, "Ping a node"), ("ping", _node_ping, "Ping a node")):
        p = sub.add_parser(name, help=text)
        p.add_argument("name")
        p.set_defaults(func=handler)


def _node_arg(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--node", default=None, help="an approved remote node ('auto': the only one)")


def _join_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("url", help="https://meet.google.com/...")
    parser.add_argument("--guest-name", default="Prometheus Agent")
    parser.add_argument("--duration", default=None, help="e.g. 30m, 2h, 90s")
    parser.add_argument("--headed", action="store_true", help="show the browser")
    parser.add_argument("--mode", choices=("transcribe", "realtime"), default="transcribe")
    _node_arg(parser)


def _transcript_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--last", type=int, default=None)
    _node_arg(parser)


def _say_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("text")
    _node_arg(parser)


def _none(parser: argparse.ArgumentParser) -> None:
    pass


def _dispatch(args: argparse.Namespace) -> Optional[int]:
    return args.func(args)


# name -> (help, parser setup, handler)
COMMANDS: dict[str, tuple[str, Callable, Callable]] = {
    "setup": ("Check the platform, browser runtime and Google sign-in", _none, setup),
    "install": ("Install the browser runtime (Playwright, Chromium, websockets)", _none, install),
    "auth": ("Sign in to Google and save the session state", _none, auth),
    "join": ("Join a Meet URL in the background", _join_parser, join),
    "status": ("Print the meeting bot's state", _node_arg, status),
    "transcript": ("Print the transcript", _transcript_parser, transcript),
    "say": ("Speak in a realtime meeting", _say_parser, say),
    "stop": ("Leave the meeting", _node_arg, stop),
    "node": ("Remote meet nodes: run, list, approve, remove, status, ping", _node_parser, _dispatch),
}
