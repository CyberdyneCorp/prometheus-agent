# LSP diagnostics

After [`write_file`](../reference/tools.md#write_file) or
[`patch`](../reference/tools.md#patch) changes a file and its
[syntax check](files-and-web.md#syntax-checks-after-a-write) is clean,
Prometheus asks the file's language server (rust-analyzer, pyright, gopls,
typescript-language-server, clangd and others) what the edit broke. Only
diagnostics the edit **introduced** are reported, never the file's
pre-existing ones:

```json
{"path": "app.py", "lint": {"status": "ok"},
 "lsp_diagnostics": ["ERROR [12:5] \"foo\" is not defined [reportUndefinedVariable] (pyright)"]}
```

Each line is `<SEVERITY> [<line>:<col>] <message> [<code>] (<server>)`.
Diagnostics are compared by severity, message and code, so moving a
pre-existing error to another line does not report it again.

A language server never fails or slows down a write beyond a small budget:

- the write waits for the verdict only while the whole tool call stays
  under 80 ms;
- a slower verdict (a server still starting, a large project) is attached
  to the session's **next tool result**, labelled
  `LSP diagnostics introduced by your earlier edit of <path>`;
- when the model ends its turn while a check is still running, the turn
  waits up to `lsp.wait_timeout_ms` (5 s) and hands any findings to the
  model as a new message before its final answer stands; earlier messages
  are never rewritten;
- a second edit of a file whose first check has not finished is compared
  against the content before the first edit.

## Where servers run

A server runs for an edited file only when:

- `lsp.enabled = true` (the default; `false` starts no server, watcher or
  background task),
- the session's terminal backend uses the host filesystem (the local
  backend; container, SSH and cloud backends get syntax checks only),
- the file is inside a **git working tree**, and its root does not match
  `lsp.exclude_roots`.

So a gateway session in your home directory, which is not a repository,
starts no server: its writes carry the syntax check only.

## Workspace trust

Some servers execute project code: rust-analyzer runs build scripts and
proc macros, gopls runs the go command, jdtls and kotlin-language-server
run Gradle. They start only in **trusted** workspaces:

- the git working tree of the directory the session was pointed at (the
  launch directory, `--cwd`, a `-w` worktree, a desktop or TUI project, or a
  gateway's `terminal.cwd`);
- any root under an entry of `lsp.trusted_workspaces` (globs, `~`
  expanded, a bare path also trusting everything below it).

Trust is fixed when the session starts. A `cd` in the terminal does not
move it, a checkout the agent cloned (or any nested checkout with its own
`.git`) is a workspace of its own, a repository at or above your home
directory never inherits trust, and cron jobs and kanban workers get no
automatic trust at all.

In an untrusted workspace only servers that execute no project code start,
with their project hooks pinned off: pyright uses the managed Python, not
the project's virtualenv; typescript-language-server uses the TypeScript
installed with it, not the workspace's; svelte-language-server starts with
`isTrusted = false`. Every other server, and every custom server, is
skipped with the log line

```text
skipped: untrusted workspace /tmp/x; add it to lsp.trusted_workspaces
```

```toml
[lsp]
trusted_workspaces = ["~/code"]
exclude_roots = ["~/src/huge-monorepo"]
```

## Sandbox

Every server runs under the [OS sandbox](sandbox.md): it reads its
workspace, the system toolchain, its toolchain directories in your home
(`~/.cargo` and `~/.rustup` for rust-analyzer, `~/go` for gopls...) and
its install directory; it writes only its cache directory under
`<profile>/cache/lsp/<id>/` (rust-analyzer's `CARGO_TARGET_DIR` and
gopls' `GOCACHE` point there); its environment is the session's child
allowlist; it reaches the network only through the session's egress
proxy. A server that tries to write the workspace is denied, and nothing a
server does changes the write that triggered it. Its stderr is logged at
DEBUG.

## Servers

The built-in registry maps file types to servers:

| Server | Files | Install | Runs project code |
|---|---|---|---|
| `rust-analyzer` | `.rs` | manual (`rustup component add rust-analyzer`) | yes |
| `pyright` | `.py`, `.pyi` | npm | no (pinned) |
| `typescript-language-server` | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.mts`, `.cts` | npm (with TypeScript) | no (pinned) |
| `gopls` | `.go` | go | yes |
| `clangd` | C, C++, Objective-C | manual | no |
| `bash-language-server` | `.sh`, `.bash` | npm | no |
| `yaml-language-server` | `.yaml`, `.yml` | npm | no |
| `dockerfile-language-server` | `Dockerfile`, `.dockerfile` | npm | no |
| `vue-language-server` | `.vue` | npm | yes |
| `svelte-language-server` | `.svelte` | npm | no (pinned) |
| `astro-language-server` | `.astro` | npm | yes |
| `intelephense` | `.php` | npm | no |
| `lua-language-server` | `.lua` | manual | yes |
| `terraform-ls` | `.tf`, `.tfvars` | manual | yes |
| `jdtls` | `.java` | manual | yes |
| `kotlin-language-server` | `.kt`, `.kts` | manual | yes |
| `elixir-ls` | `.ex`, `.exs` | manual | yes |
| `zls` | `.zig` | manual | yes |
| `clojure-lsp` | `.clj`, `.cljs`, `.cljc`, `.edn` | manual | yes |
| `haskell-language-server` | `.hs`, `.lhs` | manual | yes |
| `ocaml-lsp` | `.ml`, `.mli` | manual | yes |
| `prisma-language-server` | `.prisma` | npm | no |
| `cmake-language-server` | `CMakeLists.txt`, `.cmake` | pip | no |

A server's workspace is the outermost directory inside the git root holding
one of its root markers (`Cargo.toml`, `go.mod`, `package.json`...), else the
git root.

### Overrides and custom servers

`lsp.servers.<id>` overrides a built-in server: `disabled`, `command` (an
argv that bypasses installation), `env` (non-secret values) and
`initialization_options` (merged into the server's own). Any other key
declares a **custom server**; it needs `command` and `extensions`, accepts
`root_markers`, `language_id` and `description`, is matched before the
built-ins, is never installed automatically and counts as executing
project code (trusted workspaces only). A key that is neither is a
configuration error (exit 78).

```toml
[lsp.servers.gopls]
env = { GOFLAGS = "-mod=mod" }

[lsp.servers.mylang]
command = ["mylang-ls", "--stdio"]
extensions = [".ml2"]
```

## Installing servers

With `lsp.install_strategy = "auto"` (the default) a missing `npm`, `go` or
`pip` server is installed the first time it is needed, into
`<PROMETHEUS_HOME>/runtimes/lsp/<id>/`, when
[`runtimes.allow_lazy_install`](install-and-update.md) allows: npm packages
with the managed Node (or `lsp.package_manager = "pnpm"`/`"yarn"`), Go
modules with `go install`, pip packages into a private virtualenv of the
managed CPython. Package install scripts never run. A configured package
manager that is not installed skips the install with a notice; another
manager is never used instead. `manual` servers are never installed for
you. With `install_strategy = "manual"` only binaries on `PATH` are used.

## Timeouts and broken servers

| Key | Default | Meaning |
|---|---|---|
| `lsp.wait_mode` | `document` | `full` also asks for workspace diagnostics (other files the edit broke, as `<path>: <diagnostic>`) |
| `lsp.wait_timeout_ms` | 5000 | bound of one check and of the wait before a final answer |
| `lsp.warmup_timeout_ms` | 0 | bound of the first check against a server not running yet (`0` = `wait_timeout_ms`) |
| `lsp.broken_retry_secs` | 0 | after a server fails to start or times out, skip it for that workspace this long (`0` = until `prometheus lsp restart` or the process exits) |
| `lsp.idle_timeout_secs` | 600 | stop a server idle this long (`0` = never); it restarts on demand |

## The `lsp` command

```text
prometheus lsp status              running servers: workspace, pid, uptime, state
prometheus lsp list [--installed-only]
prometheus lsp install <id>        npm/go/pip servers; prints instructions for manual ones
prometheus lsp install-all
prometheus lsp restart [<id>]      stop servers and clear broken marks (every process of the profile)
prometheus lsp which <id>          the binary it would run; exit 66 when none is found
```

In a fresh profile nothing runs yet, and a server that is neither
installed nor on `PATH` is named with how to get it:

```console
$ prometheus lsp status
no running language servers
$ prometheus lsp list
rust-analyzer <...> trusted workspaces only
pyright npm <...> Python
$ prometheus lsp which zls # exits 66
error: no zls binary found (not installed under runtimes/lsp and not on PATH)
hint: install it manually: install zls (https://github.com/zigtools/zls)
$ prometheus lsp which nope # exits 2
error: unknown language server 'nope'
```

`status` marks servers that start only in trusted workspaces. `--json`
prints `prometheus.lsp_status.v1` and `prometheus.lsp_list.v1` documents.
The desktop app and dashboard read the same data through RPC
[`lsp.status`](../reference/rpc.md).
