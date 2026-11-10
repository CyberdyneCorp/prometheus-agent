# Session recordings and replay

Every model call is content-addressed: its `request_hash` is
`sha256:<hex>` of the canonical request (provider, wire API, model, system
prompt, messages, tool definitions, tool choice, sampling parameters and
response format, as JSON with sorted keys). Transport details (request
ids, headers, the streaming flag) and prompt-cache markers are not part of
it, so the same request always has the same hash. The usage ledger and the
trajectory's `model.call` record it.

## The model-call cache

`[replay.cache]` stores successful responses by request hash under
`<profile>/cache/model-calls/` (zstd-compressed, mode 0600), with the
event stream in its original chunks and the usage:

| Key | Default | Meaning |
|---|---|---|
| `mode` | `off` | `read` serves stored responses, `write` stores new ones, `read_write` both |
| `namespace` | `default` | entries of different namespaces never serve each other |
| `max_bytes` | 4 GiB | least-recently-used entries are evicted above it |

A served response costs nothing: its ledger row has `cache_hit = true` and
`cost_usd = 0`. Failed responses are never stored, and entries hold no
request and no credential. The offline demo uses `read_write`.

```console
$ prometheus cache stats
model-call cache: <...> entries, <...>
$ prometheus cache prune --older-than 30d
removed <...> entries (<...>)
$ prometheus cache clear --namespace default
removed <...> entries (<...>)
```

## Recordings

While trajectories are on (`observability.trajectories.enabled`), each
session is recorded beside its trajectory in
`<profile>/recordings/<session>.jsonl`: every prompt build layer by layer,
every model call (its canonical request and its response, failures
included) and every tool result. It is deleted with the session's
trajectory.

`prometheus replay export <session> [-o FILE] [--include-workspace]
[--lineage]` writes a `.pmtrec` file: a zstd-compressed tar whose
`manifest.json` lists the sha256 of every member. It holds the
trajectory, the responses by request hash, the tool results, the initial
state (the configuration the session ran with, which names secrets but
holds no value; the toolsets and tool schemas; hashes of the skills and
memory in its prompt; the build) and a manifest of the workspace (path,
size, sha256, as the session found it: changes its file tools made are
undone from the file snapshots). The workspace manifest leaves out
hidden and ignored files (`.gitignore` and the like) and lists at most
10,000 files; a larger workspace is cut there and the manifest says
`truncated: true`. File contents are included only with
`--include-workspace`; `--lineage` adds every descendant session
(subagents, forks). Every member is redacted like a trajectory.

## Replay

`prometheus replay run <recording> [--mode strict|tools-live|model-live]
[--on-divergence stop|continue] [--out DIR]` re-executes the recording in
a home of its own under `DIR` (by default a new directory under the
profile's `cache/replay/`), with the recorded configuration, the session's
recorded id, and each recorded user message as one turn:

| Mode | Model calls | Tools |
|---|---|---|
| `strict` (default) | served from the recording | served from the recording; nothing runs |
| `tools-live` | served from the recording | run for real in `DIR/workspace`, a copy of the recorded workspace (needs `--include-workspace`) |
| `model-live` | sent to the configured provider | served from the recording |

The prompt is rebuilt by the current build, with the recorded text of
every layer that comes from the recorded machine (identity, skills,
memory, project context, the session and runtime lines): what can differ
is the harness's own text, the layer order and the tool schemas. A strict
replay has no secrets (placeholders stand in for the names the
configuration references), no plugins, MCP servers or language servers,
and never runs a tool: a call without a recorded result is a divergence.

At the first model request whose hash the recording lacks, or the first
tool call whose name or arguments differ from the recorded call, the
replay prints `divergence at step <n> (model.call)` (or `tool.call`) and a
unified diff of the canonical forms (at most 200 lines), then stops, or
with `--on-divergence continue` sends everything after it live. A strict
replay also compares its trajectory with the recorded one, ids, times and
costs aside. It exits 0 when the replay matched, 1 on a divergence or a
different trajectory, and 65 for a malformed or tampered recording
(`recording integrity check failed: <member>`).

`prometheus replay bless <recording>` re-records a recording after an
intended change: a strict replay that continues live from the first
divergence (the recorded mock script answers, tools run in the recorded
workspace), exported again over the file; it prints the diff it accepts.

## The regression gates

`tests/golden/*.pmtrec` are golden recordings (a plain chat, a multi-tool
turn, an approval denial, a compression, a delegation to two subagents
and a budget refusal), recorded from `tests/golden/sources/` with
`cargo xtask goldens --record`. `scripts/check` replays each strictly
(`cargo xtask goldens`) and fails naming every one that diverges; an
intended change is blessed with `prometheus replay bless` or re-recorded.

The binary bundles an eval suite named `smoke`: offline tasks on the mock
provider (tool calling, parallel tool calls, a file edit, multi-step
terminal use, an approval denial, clarify, delegation, compression, a
skill, a memory write, a budget refusal, an interrupt and error recovery),
each with an `exact` grader. The binary runs it itself with the hidden
`prometheus smoke-suite` command: `scripts/check` runs it on the Tier 1
targets (`cargo xtask smoke`), and the offline smoke run
(`scripts/ci/offline-demo`, after `prometheus demo`) runs it on every CI
target, the Tier 2 Windows and Termux builds included, where there is no
cargo. The interrupt task sends SIGINT, or on Windows Ctrl+Break to the
run's own console process group; a task needing a capability the
platform lacks would be skipped and listed.
