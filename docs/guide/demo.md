# The offline demo

`prometheus demo` shows the agent working end to end without a provider,
an API key or the network. With the scripted `mock` provider it runs a
bundled scenario: a small ledger repository whose balance script adds
debits instead of subtracting them. The agent reads the README, runs the
script through the terminal tool (`Total: 235.35`), reads the script, finds
the sign bug, fixes it with [`patch`](../reference/tools.md#patch), re-runs the script and reports
`Total: 85.15`.

```console
$ prometheus demo
[USER]
[ACTION] read_file({"path":"README.md"})
[ACTION] terminal(<...>)
[ACTION] patch(<...>)
Total: 85.15
demo passed in <...> s
```

`prometheus demo --output DIR --keep` runs in `DIR` (new or empty) and keeps
it; without `--output` the demo uses a fresh temporary directory.

Each step is printed in the canonical trajectory rendering (`[USER]`,
`[ASSISTANT]`, `[ACTION]`, `[OBSERVATION]`, as `prometheus trajectory show`
prints it), followed by `demo passed in <seconds> s`. The command exits 0
only when the final answer reports `Total: 85.15`; otherwise it prints
`demo failed: <reason>` and exits 1. A turn longer than 30 seconds is
stopped and fails.

The session is a real one-shot chat session (the same turn loop, tools,
approval gate, sandbox and trajectory writer as `prometheus chat -z`), run
in a Prometheus home of its own under the demo's directory:

| Path | Contents |
|---|---|
| `DIR/home` | the demo's home: one `default` profile with the session and its trajectory |
| `DIR/ledger` | the repository the agent fixed |
| `DIR/mock.toml` | the mock script that plays the model |

The demo never reads or writes your homes or profiles: `--home`, `-p`,
`--set` and `PROMETHEUS_HOME` are ignored, it writes no log files, and
nothing from your machine (context files, user configuration) shapes the
prompt. Without `--keep` it removes everything it created; an empty
`--output` directory you named is emptied rather than deleted, and a
non-empty one is refused (`demo.output_not_empty`, exit 2). With `--keep`
the last line names the kept directory and the command that replays the
steps:

```sh
prometheus --home DIR/home trajectory show <session-id>
```

On a platform without an OS sandbox backend (Android/Termux), commands
run only with `security.sandbox.allow_degraded = ["all"]`; the demo sets
that on its own temporary profile and prints a `note:` line saying its
scripted commands run unconfined.

The balance script is POSIX `sh` (integer arithmetic only) and PowerShell
on Windows, so the demo needs no Python, Node.js or other runtime. CI runs
it with networking disabled on every Tier 1 target and as the offline smoke
run of the Tier 2 targets.
