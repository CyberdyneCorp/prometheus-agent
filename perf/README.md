# Performance gate

`cargo xtask perf` builds the release binary and checks it against the
startup, memory and size budgets (spec: workspace-architecture, Startup time
budget; Memory and size budgets):

| Measurement | How | Absolute budget |
|---|---|---|
| Startup | 30 timed runs of `prometheus --version` after 3 warm-up runs; p95 checked | 20 ms |
| Peak RSS | `getrusage(RUSAGE_CHILDREN)` in the small `perf-sampler` binary | 120 MiB |
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
shared runner. The chat and daemon budgets join when those commands land (M1).

```sh
cargo xtask perf                          # check against perf/baseline.json
cargo xtask perf --runs 100               # more samples
cargo xtask perf --update-baseline        # record this platform's entry (budgets still apply)
cargo xtask perf --baseline PATH          # compare with another baseline file
```

`scripts/check --perf [ARGS...]` runs exactly `cargo xtask perf [ARGS...]`.

## In CI

`scripts/check` runs the gate as its last step, against the checked-in
`perf/baseline.json`; on a platform without an entry there only the absolute
budgets apply. The regression check against `main` runs in its own job,
`perf` in `.github/workflows/ci.yml`, on the reference runner (GitHub-hosted
`ubuntu-24.04`, x86_64), so every comparison is between runs on the same
kind of machine:

1. On every push to `main`, run `scripts/check --perf --update-baseline --baseline target/perf-main.json`
   and upload `target/perf-main.json` as the `perf-baseline` artifact.
2. On every pull request, download the latest `perf-baseline` artifact from
   `main` and run `scripts/check --perf --baseline target/perf-main.json`; a
   non-zero exit fails the job. Without an artifact the absolute budgets
   still apply, and the checked-in `perf/baseline.json` is used as a fallback.

The checked-in `perf/baseline.json` starts empty; it holds entries only when
someone records them deliberately with `--update-baseline`.

For a local before/after comparison:

```sh
git stash && cargo xtask perf --update-baseline --baseline target/perf-local.json
git stash pop && cargo xtask perf --baseline target/perf-local.json
```
