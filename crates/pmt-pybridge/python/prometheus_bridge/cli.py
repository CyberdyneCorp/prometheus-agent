"""``prometheus plugins run <plugin> <name> [args...]``: the argparse tree a
plugin registered, run against argv with output streamed to the host."""

import argparse
import sys
import traceback

from . import state
from .context import HostError, host


class ConsoleOut:
    """sys.stdout / sys.stderr of a command: every write goes to the host."""

    def __init__(self, stream):
        self._stream = stream
        self.encoding = "utf-8"

    def write(self, text):
        if text:
            host("console.write", {"stream": self._stream, "text": str(text)})
        return len(text or "")

    def flush(self):
        pass

    def isatty(self):
        return False


class ConsoleIn:
    """sys.stdin of a command: lines read from the host's terminal."""

    encoding = "utf-8"

    def readline(self, *_):
        try:
            return host("console.readline", {}) or ""
        except HostError:
            return ""

    def read(self, *_):
        out = []
        while True:
            line = self.readline()
            if not line:
                return "".join(out)
            out.append(line)

    def __iter__(self):
        while True:
            line = self.readline()
            if not line:
                return
            yield line

    def isatty(self):
        return False


def run(name, argv, real_stderr):
    """Runs CLI command ``name``; returns its exit code."""
    entry = state.REGISTRY["cli_commands"].get(name)
    if entry is None:
        raise KeyError(name)
    saved = sys.stdout, sys.stderr, sys.stdin
    sys.stdout, sys.stderr, sys.stdin = ConsoleOut("stdout"), ConsoleOut("stderr"), ConsoleIn()
    try:
        parser = argparse.ArgumentParser(prog=f"prometheus plugins run {state.PLUGIN_ID} {name}",
                                         description=entry.get("description") or entry.get("help"))
        entry["setup_fn"](parser)
        try:
            namespace = parser.parse_args(argv)
            handler = entry.get("handler_fn") or getattr(namespace, "func", None)
            result = handler(namespace) if handler else None
        except SystemExit as exit_:
            return _code(exit_.code)
        return _code(result)
    except Exception:  # noqa: BLE001 - an uncaught exception is exit 70
        traceback.print_exc(file=real_stderr)
        sys.stderr.write(f"plugin command failed; see prometheus bridge logs {state.PLUGIN_ID}\n")
        return 70
    finally:
        sys.stdout, sys.stderr, sys.stdin = saved


def _code(value):
    if value is None:
        return 0
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    return 1
