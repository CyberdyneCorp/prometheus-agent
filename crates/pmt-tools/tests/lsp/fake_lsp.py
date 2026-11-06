"""A stand-in language server for the LSP diagnostics tests.

Every line containing BAD_<name> gets an error `"<name>" is not defined`
and every line containing NOTE gets a warning, published with the
document's version; a line `BREAKS <file>` also publishes an error for the
sibling <file> (workspace diagnostics). Behaviour switches come from the environment:

FAKE_LSP_LOG      append one line per start and per document sync
FAKE_LSP_DELAY_MS wait this long before each publish
FAKE_LSP_HANG     never answer `initialize`
FAKE_LSP_WRITE    try to create this file at start; the outcome is logged
                  and published as an information diagnostic of every
                  document
"""

import json
import os
import re
import sys
import time

LOG = os.environ.get("FAKE_LSP_LOG")
WRITE_OUTCOME = []


def log(line):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as out:
            out.write(line + "\n")


def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("ascii").strip()
        if not line:
            break
        name, _, value = line.partition(":")
        if name.lower() == "content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


def diagnostics(text):
    found = []
    for number, line in enumerate(text.split("\n")):
        for match in re.finditer(r"BAD_(\w+)", line):
            found.append({
                "range": {"start": {"line": number, "character": match.start()},
                          "end": {"line": number, "character": match.end()}},
                "severity": 1,
                "code": "undefined",
                "message": '"%s" is not defined' % match.group(1),
            })
        if "NOTE" in line:
            found.append({
                "range": {"start": {"line": number, "character": 0},
                          "end": {"line": number, "character": 4}},
                "severity": 2,
                "message": "a note",
            })
    for outcome in WRITE_OUTCOME:
        found.append({
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 0, "character": 1}},
            "severity": 3,
            "message": outcome,
        })
    return found


def publish(uri, version, text):
    delay = int(os.environ.get("FAKE_LSP_DELAY_MS", "0"))
    if delay:
        time.sleep(delay / 1000)
    for match in re.finditer(r"BREAKS (\S+)", text):
        sibling = uri.rsplit("/", 1)[0] + "/" + match.group(1)
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": sibling, "diagnostics": [{
                  "range": {"start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1}},
                  "severity": 1, "message": "broken by an edit elsewhere"}]}})
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "version": version, "diagnostics": diagnostics(text)}})


def main():
    log("start " + os.getcwd())
    target = os.environ.get("FAKE_LSP_WRITE")
    if target:
        try:
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w", encoding="utf-8") as out:
                out.write("escaped")
            WRITE_OUTCOME.append("write allowed")
        except OSError as err:
            WRITE_OUTCOME.append("write denied: %s" % err.__class__.__name__)
        log(WRITE_OUTCOME[0])
    while True:
        message = read()
        if message is None:
            return
        method = message.get("method")
        params = message.get("params") or {}
        if method == "initialize":
            if os.environ.get("FAKE_LSP_HANG"):
                continue
            log("options " + json.dumps(params.get("initializationOptions")))
            send({"jsonrpc": "2.0", "id": message["id"],
                  "result": {"capabilities": {"textDocumentSync": 1}}})
        elif method == "textDocument/didOpen":
            doc = params["textDocument"]
            log("open %s %d" % (os.path.basename(doc["uri"]), doc["version"]))
            publish(doc["uri"], doc["version"], doc["text"])
        elif method == "textDocument/didChange":
            doc = params["textDocument"]
            log("change %s %d" % (os.path.basename(doc["uri"]), doc["version"]))
            publish(doc["uri"], doc["version"], params["contentChanges"][-1]["text"])
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "exit":
            return
        elif "id" in message:
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})


main()
