"""A sample benchmark adapter: JSON-RPC 2.0 over stdio, one message per line.

It logs every method it is asked to `calls.log` beside itself, and while
preparing a task it tries to push a score to the host, logging the error
code it gets back (the host refuses with -32002 during execution).
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
TASKS = {
    "e1": {"prompt": "sum ledger e1", "ledger": "+100.00\n-14.85\n"},
    "e2": {"prompt": "sum ledger e2", "ledger": "+20.25\n"},
}


def log(line):
    with open(os.path.join(HERE, "calls.log"), "a", encoding="utf-8") as out:
        out.write(line + "\n")


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def push_score(task_id):
    send({"jsonrpc": "2.0", "id": "push-" + task_id, "method": "bench.report_score",
          "params": {"task_id": task_id, "n_passed": 1, "n_criteria": 1}})
    reply = json.loads(sys.stdin.readline())
    log("push: %s" % reply.get("error", {}).get("code"))


def answer(method, params):
    if method == "bench.describe":
        return {"name": "example", "version": "1.0", "domains": ["accounting"],
                "leak_globs": ["answers*"]}
    if method == "bench.tasks":
        return ["e1"] if params.get("split") == "dev" else ["e1", "e2"]
    if method == "bench.task":
        task = TASKS[params["task_id"]]
        return {"prompt": task["prompt"], "domain": "accounting", "timeout_secs": 60,
                "files": [{"path": "ledger.txt", "content": task["ledger"]}]}
    if method == "bench.prepare":
        with open(os.path.join(params["workspace"], "notes.txt"), "w", encoding="utf-8") as out:
            out.write("prepared by the adapter\n")
        push_score(params["task_id"])
        return {}
    if method == "bench.deliverables":
        return {}
    if method == "bench.preflight":
        return {"ok": True}
    if method == "bench.score":
        if params["task_id"] == "e2":
            return None
        path = os.path.join(params["run_dir"], "deliverables", "answer.md")
        with open(path, encoding="utf-8") as text:
            passed = "85.15" in text.read()
        return {"n_passed": 1 if passed else 0, "n_criteria": 1}
    raise KeyError(method)


for line in sys.stdin:
    request = json.loads(line)
    log(request["method"])
    try:
        send({"jsonrpc": "2.0", "id": request["id"], "result": answer(request["method"], request.get("params", {}))})
    except KeyError as missing:
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": str(missing)}})
