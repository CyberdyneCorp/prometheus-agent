"""A sample benchmark adapter (Prometheus extension ``register_bench_adapter``):
the two-task ledger benchmark the eval tests run as ``bench:example``.

It reaches files only through its ``host`` argument (the sidecar is
sandboxed): ``prepare`` tries to push a score (refused with -32002 during
``prometheus eval run``) and writes a note into the workspace;
``deliverables`` records every method asked so far and that refusal in the
run directory; ``score`` grades ``deliverables/answer.md`` and gives ``e2``
a null score."""

TASKS = {
    "e1": {"prompt": "sum ledger e1", "ledger": "+100.00\n-14.85\n"},
    "e2": {"prompt": "sum ledger e2", "ledger": "+20.25\n"},
}


class LedgerBench:
    name = "example"

    def __init__(self):
        self.calls = []
        self.push = "none"

    def describe(self):
        self.calls.append("bench.describe")
        return {"name": "example", "version": "1.0", "domains": ["accounting"], "leak_globs": ["answers*"]}

    def preflight(self):
        self.calls.append("bench.preflight")
        return {"ok": True}

    def tasks(self, split=None):
        self.calls.append("bench.tasks")
        return ["e1"] if split == "dev" else ["e1", "e2"]

    def task(self, task_id):
        self.calls.append("bench.task")
        task = TASKS[task_id]
        return {"prompt": task["prompt"], "domain": "accounting", "timeout_secs": 60,
                "files": [{"path": "ledger.txt", "content": task["ledger"]}]}

    def prepare(self, task_id, workspace, host):
        self.calls.append("bench.prepare")
        try:
            host.request("bench.report_score", {"task_id": task_id, "n_passed": 1, "n_criteria": 1})
        except Exception as refused:  # noqa: BLE001 - the refusal is what we record
            self.push = str(getattr(refused, "rpc", refused))
        host.write("notes.txt", "prepared by the adapter\n")
        return {}

    def deliverables(self, task_id, run_dir, host):
        self.calls.append("bench.deliverables")
        host.write("adapter-calls.txt", "\n".join(self.calls) + "\npush: " + self.push + "\n")
        return {}

    def score(self, task_id, run_dir, host):
        self.calls.append("bench.score")
        if task_id == "e2":
            return None
        passed = "85.15" in host.read("deliverables/answer.md").decode("utf-8")
        return {"n_passed": 1 if passed else 0, "n_criteria": 1}


def register(ctx):
    ctx.register_bench_adapter(LedgerBench())
