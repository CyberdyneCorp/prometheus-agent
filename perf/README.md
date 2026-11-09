# Performance gate

`cargo xtask perf` builds the release binary and checks it against the
startup, memory and size budgets (spec: workspace-architecture, Startup time
budget; Memory and size budgets):

| Measurement | How | Absolute budget |
|---|---|---|
| Startup | 30 timed runs of `prometheus --version` after 3 warm-up runs; p95 checked | 20 ms |
| Chat prompt | 30 timed starts of `prometheus chat --provider mock` in a pseudo-terminal (fresh home under `target/`), until the `› ` prompt is drawn and accepts keystrokes; p95 checked (Unix) | 150 ms |
| Peak RSS | `getrusage(RUSAGE_CHILDREN)` in the small `perf-sampler` binary, for the `--version` runs | 120 MiB |
| Chat peak RSS | the same, over the chat starts and three `chat -z hi --provider mock` runs (Unix) | 120 MiB |
| Binary size | stripped release `prometheus`, at the path cargo reports | 80 MiB |

Peak RSS is sampled from `xtask/src/bin/perf-sampler.rs`, a std-only
binary, because on Linux a child spawned with `posix_spawn` (vfork) starts
its `maxrss` at the spawner's memory high-water mark. The sampler is small,
and a peak that does not rise above its own high-water mark is reported as
"not measurable" rather than recorded as the binary's.

It also fails when a measurement grows more than 15% over the entry for the
current platform (`<os>-<arch>`) in `perf/baseline.json`. Startup uses the
median for that comparison, and growth under 1 ms (startup) or 1 MiB (RSS)
never counts, because a millisecond-scale run jitters by more than 15% on a
shared runner. Binary growth under 4 MiB (5% of the size budget) never counts
either: size does not jitter, but against a near-empty binary any real
dependency is a large percentage (M1's first runtime crates took 0.62 MiB to
1.21 MiB, +94%), so a pure percentage gate would fail on every addition. The chat
prompt median and the chat peak RSS follow the same rule with the startup
and RSS floors; a baseline recorded before they were measured has no chat
entries, and then only their absolute budgets apply. The daemon budgets
(`/readyz` latency, idle RSS) join with the daemon in M2.

```sh
cargo xtask perf                          # check against perf/baseline.json
cargo xtask perf --runs 100               # more samples
cargo xtask perf --update-baseline        # record this platform's entry (budgets still apply)
cargo xtask perf --baseline PATH          # compare with another baseline file
```

`scripts/check --perf [ARGS...]` runs exactly `cargo xtask perf [ARGS...]`.

## In CI

A local `scripts/check` runs the gate as its last step, against the checked-in
`perf/baseline.json`; on a platform without an entry there only the absolute
budgets apply. CI's platform jobs run `scripts/check --no-perf`, because the
budgets are defined for the reference runner and shared runners are too noisy
to gate on. The budgets and the regression check against the base branch run in their own job,
`perf` in `.github/workflows/ci.yml`, on the reference runner (GitHub-hosted
`ubuntu-24.04`, x86_64), so every comparison is between runs on the same
kind of machine:

1. On every push to `main`, run `scripts/check --perf --update-baseline --baseline target/perf-main.json`,
   which checks the absolute budgets.
2. On every pull request, check out the base branch in a worktree, record
   its measurements with `scripts/check --perf --update-baseline --baseline target/perf-base.json`,
   then run `scripts/check --perf --baseline target/perf-base.json` on the
   head; a non-zero exit fails the job. Both measurements come from the same
   machine minutes apart. A baseline recorded by another run lands on another
   runner: the chat prompt median of one M2 branch moved between 22.6 and
   32.5 ms across its runs, so comparing across runners failed the 15% gate
   at random. Stacked pull requests compare with the branch they target.
   Without a base branch (`workflow_dispatch`) the checked-in
   `perf/baseline.json` applies.
3. A pull request whose growth is deliberate re-baselines in the open: it
   commits the reference runner's measurements (the `perf (linux-x86_64):`
   line of its own perf job) to `perf/reference.json`, and while the pull
   request changes that file the job compares with it instead of the base
   branch.
   The new numbers are part of the reviewed diff; the absolute budgets
   always apply. The M1 branch did this five times. First (0.62 -> 8.49 MiB binary,
   2.49 -> 4.28 MiB peak RSS: SQLite, the HTTP client, the keychain and
   the async runtime), measured by CI run 41021271519. Then when the
   `prometheus` binary itself started calling the HTTP client (8.49 ->
   13.05 MiB binary, 4.28 -> 4.96 MiB peak RSS: `model list --refresh`,
   local-server probes and endpoint tests link reqwest, rustls and ring,
   which chat needs anyway), measured by CI run 41112215077. Then when the
   tool system landed (13.05 -> 15.37 MiB binary, 4.96 -> 6.00 MiB peak
   RSS: the `tools`, `toolsets` and `approvals` commands link the tool
   registry, dispatcher and approval gate, which chat needs anyway, and add
   their subcommands to the clap tree every launch builds; nothing in them
   runs eagerly, their regexes are lazy), measured by CI run 41155059146.
   Then when the vision and browser tools landed (15.37 -> 19.46 MiB
   binary, 6.00 -> 6.15 MiB peak RSS: image decoding and SVG
   rasterization for `vision_analyze` and the DevTools WebSocket client of
   `browser_exec`, registered as built-in tools; nothing in them runs at
   startup), measured by CI run 41169839367. Then when `prometheus chat`
   landed (19.46 -> 31.30 MiB binary, 6.15 -> 7.62 MiB peak RSS: session
   start links the whole runtime the earlier commands only named: the turn
   loop with compression, every provider wire format and the streaming
   caller, the memory and skills sessions, the local terminal, file and
   web backends and the line editor; `--version` still runs none of it),
   measured by CI run 41211014328.
   The CI line
   prints MiB with two decimals, so the committed byte counts are those
   values times 1048576; the rounding (under 6 KiB) is far inside the 15%
   tolerance.

Release builds use fat LTO (`[profile.release]` in `Cargo.toml`). The
messaging gateway's adapters took the stripped binary 17% over its base on
the reference runner (38.01 -> 44.30 MiB, CI run 41542594747), over the
15% gate; first-party mocks had already left release builds and per-crate
`opt-level = "s"` changed nothing under thin LTO. Fat LTO optimizes all
crates as one unit and dropped the same binary by 9.1% (macOS: 36.74 ->
33.39 MiB) with unchanged startup (`--version` median 8.1 ms thin, 7.9 ms
fat, same machine), for a release build about twice as long (2.5 -> 5
minutes locally). The pull request that switched (m2-gateway) is
compared with a thin-LTO base, so its gate saw the gateway's growth net of
that saving (about +7%); every branch after it builds base and head alike
with fat LTO, so their growth is measured in full again.

The WASM plugin host (m2-extensions) re-baselined `perf/reference.json`
deliberately: wasmtime with the Cranelift compiler (design D7; plugins are
compiled at load, so the compiler ships in the binary) took the stripped
binary from 42.97 to 54.07 MiB (+26%) on the reference runner, while
startup (2.67 -> 2.73 ms median), peak RSS (9.10 -> 9.49 MiB) and the chat
prompt (34.69 -> 33.59 ms) stayed put: nothing of the engine runs unless a
plugin is enabled. Measured by CI run 41610015894. Cranelift's host ISA
and the Pulley interpreter come with the engine's `cranelift` feature;
there is no smaller configuration that still compiles components at load.

The MCP client and server (m2-extensions, group 31) re-baselined it again,
deliberately: rmcp with the client and server roles over stdio and
streamable HTTP (both directions, since `mcp serve --http` and HTTP MCP
servers are specified), its JSON-RPC model types, the OAuth sign-in,
client certificates (PKCS#8 decryption), sampling, elicitation and the
catalog took the stripped binary from 54.07 to 64.12 MiB (+19%) on the
reference runner, after the hooks and toolset groups between them had used
1.8 MiB of the 15% (55.83 MiB). Nothing of it runs before a session starts
an MCP server: startup stayed within the 1 ms floor (2.73 -> 3.52 ms
median), peak RSS moved 9.49 -> 10.48 MiB and the chat prompt 33.59 ->
37.77 ms. No rmcp feature can go: each one backs a specified transport or
role, and the client and server share the model types. Measured by CI run
41757484815.

Local models (m2-models-media, group 48: the llama.cpp engine index, the
hardware-priced catalog, resumable downloads, the model library, the managed
`llama-server` and the `local` commands and RPC methods) took the stripped
binary on the reference runner from 79.22 to 80.19 MiB, over the 80 MiB
absolute budget (CI run 42621963414); no new third-party crate came with
it. The release profile now optimizes for size (`opt-level = "s"`) instead
of speed: with fat LTO the stripped binary went from 63.17 to 45.29 MiB on
macOS (-28%), while the `--version` median (4.59 -> 4.34 ms), the chat
prompt median (129.50 -> 130.65 ms on that machine) and peak RSS (11.42 ->
10.95 MiB) stayed put. Per-crate `opt-level = "s"` had not helped (above);
the whole-profile setting does, because the LTO pass optimizes the single
merged unit at the profile's level. The cost is throughput in CPU-bound
code (Cranelift compiling a plugin, hashing a download), which is small next
to the network and model latency a turn waits on; startup and RSS, the
budgeted quantities, do not regress.

The checked-in `perf/baseline.json` starts empty; it holds entries only when
someone records them deliberately with `--update-baseline`.

For a local before/after comparison:

```sh
git stash && cargo xtask perf --update-baseline --baseline target/perf-local.json
git stash pop && cargo xtask perf --baseline target/perf-local.json
```
