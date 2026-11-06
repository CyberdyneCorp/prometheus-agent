//! The `prometheus_tools` module a script imports: one function per stub
//! tool, each writing a request into the channel directory and waiting
//! for the host's answer (the tool's text). A refused call raises
//! `ToolError` with the host's message.

/// The module source for `tools`.
#[must_use]
pub fn module(tools: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(PRELUDE);
    for tool in tools {
        let _ = write!(
            out,
            "\n\ndef {tool}(**kwargs):\n    return _call({tool:?}, kwargs)\n"
        );
    }
    let _ = write!(out, "\n\nAVAILABLE = {tools:?}\n");
    out
}

const PRELUDE: &str = r#""""Tool stubs of this execute_code run (generated)."""
import itertools
import json
import os
import time

_DIR = os.environ.get("PMT_RPC_DIR", "")
_TOKEN = os.environ.get("PMT_RPC_TOKEN", "")
_IDS = itertools.count()


class ToolError(RuntimeError):
    """The host refused or failed a tool call."""


def _call(tool, args):
    name = os.path.join(_DIR, "%08d" % next(_IDS))
    with open(name + ".req.tmp", "w", encoding="utf-8") as f:
        json.dump({"token": _TOKEN, "tool": tool, "args": args}, f)
    os.replace(name + ".req.tmp", name + ".req")
    answer = name + ".resp"
    while not os.path.exists(answer):
        time.sleep(0.01)
    with open(answer, encoding="utf-8") as f:
        reply = json.load(f)
    os.remove(answer)
    if not reply.get("ok"):
        raise ToolError(reply.get("result", "tool call failed"))
    return reply.get("result", "")


def call(tool, **kwargs):
    """Calls any tool by name (refused unless it is one of AVAILABLE)."""
    return _call(tool, kwargs)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_stub_tool_gets_a_function() {
        let source = module(&["read_file".to_owned(), "search_files".to_owned()]);
        assert!(
            source.contains("def read_file(**kwargs):\n    return _call(\"read_file\", kwargs)")
        );
        assert!(source.contains("AVAILABLE = [\"read_file\", \"search_files\"]"));
        assert!(!source.contains("def terminal("));
    }
}
