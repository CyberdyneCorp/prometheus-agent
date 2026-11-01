# browser_exec prelude: helpers bound to the session's browser. Each helper
# sends one JSON request on the real stdout and reads the JSON reply from
# stdin; the model's code runs with stdout and stderr captured.
import io
import json
import sys
import traceback

_rpc_in = sys.stdin
_rpc_out = sys.__stdout__


class BrowserError(Exception):
    """A browser helper failed."""


def _call(name, *args):
    _rpc_out.write(json.dumps({"call": name, "args": list(args)}) + "\n")
    _rpc_out.flush()
    line = _rpc_in.readline()
    if not line:
        raise SystemExit("browser_exec: the host closed the helper channel")
    reply = json.loads(line)
    if "error" in reply:
        raise BrowserError(reply["error"])
    return reply.get("ok")


def new_tab(url=None):
    return _call("new_tab", url)


def goto_url(url):
    return _call("goto_url", url)


def wait_for_load(timeout=30):
    return _call("wait_for_load", timeout)


def page_info():
    return _call("page_info")


def js(expression):
    return _call("js", expression)


def fill_input(selector, text):
    return _call("fill_input", selector, text)


def click_at_xy(x, y):
    return _call("click_at_xy", x, y)


def capture_screenshot(path=None):
    return _call("capture_screenshot", path)


def cdp(method, params=None):
    return _call("cdp", method, params or {})


def ensure_real_tab():
    return _call("ensure_real_tab")


def _main():
    code = json.loads(_rpc_in.readline())["code"]
    names = {
        "__name__": "__main__",
        "BrowserError": BrowserError,
        "new_tab": new_tab,
        "goto_url": goto_url,
        "wait_for_load": wait_for_load,
        "page_info": page_info,
        "js": js,
        "fill_input": fill_input,
        "click_at_xy": click_at_xy,
        "capture_screenshot": capture_screenshot,
        "cdp": cdp,
        "ensure_real_tab": ensure_real_tab,
    }
    captured = io.StringIO()
    sys.stdout = captured
    sys.stderr = captured
    error = None
    try:
        exec(compile(code, "<browser_exec>", "exec"), names)
    except BaseException:
        error = traceback.format_exc()
    sys.stdout = sys.__stdout__
    sys.stderr = sys.__stderr__
    _rpc_out.write(json.dumps({"done": True, "output": captured.getvalue(), "error": error}) + "\n")
    _rpc_out.flush()


_main()
