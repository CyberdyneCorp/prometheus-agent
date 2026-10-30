# Performance gate

`cargo xtask perf` builds the release binary and checks it against the
startup, memory and size budgets (spec: workspace-architecture, Startup time
budget; Memory and size budgets):

| Measurement | How | Absolute budget |
|---|---|---|
| Startup | 30 timed runs of `prometheus --version` after 3 warm-up runs; p95 checked | 20 ms |
| Peak RSS | `getrusage(RUSAGE_CHILDREN)` in a dedicated sampler process | 120 MiB |
| Binary size | stripped `target/release/prometheus` | 80 MiB |

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

## In CI

The gate is not in `scripts/check`: wall-time samples on a busy developer
machine are too noisy for a blocking pre-push hook. CI runs it as its own
job on the reference runner (GitHub-hosted `ubuntu-latest`, x86_64):

1. On every push to `main`, run `cargo xtask perf --update-baseline --baseline target/perf-main.json`
   and upload `target/perf-main.json` as the `perf-baseline` artifact.
2. On every pull request, download the latest `perf-baseline` artifact from
   `main` and run `cargo xtask perf --baseline target/perf-main.json`; a
   non-zero exit fails the job. Without an artifact the absolute budgets
   still apply, and the checked-in `perf/baseline.json` is used as a fallback.

The checked-in `perf/baseline.json` starts empty; it holds entries only when
someone records them deliberately with `--update-baseline`.

For a local before/after comparison:

```sh
git stash && cargo xtask perf --update-baseline --baseline target/perf-local.json
git stash pop && cargo xtask perf --baseline target/perf-local.json
```
