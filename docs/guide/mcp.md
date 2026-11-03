# MCP servers

Prometheus speaks the [Model Context Protocol](https://modelcontextprotocol.io)
both ways: it connects to external MCP servers and offers their tools to the
model, and `prometheus mcp serve` offers Prometheus tools and skills to other
MCP clients.

## Connecting servers

Servers are configured per profile in `config.toml`:

```toml
[mcp.servers.github]
transport = "stdio"                 # a local program speaking MCP on stdio
command = "github-mcp-server"
args = ["stdio"]
env_secrets = ["GITHUB_TOKEN"]      # from the secrets store, same name
trust = "untrusted"                 # the default
[mcp.servers.github.capabilities]
network = ["api.github.com"]
[mcp.servers.github.tools]
exclude = ["delete_*"]

[mcp.servers.docs]
transport = "http"                  # a streamable HTTP endpoint
url = "https://docs.example.com/mcp"
auth_secret = "DOCS_TOKEN"          # sent as `Authorization: Bearer ...`
```

or from the command line (`--args` takes every word after it, so give it
last):

```sh
prometheus mcp add github --command github-mcp-server --env-secret GITHUB_TOKEN --args stdio
prometheus mcp test github     # start it: handshake, tool count, latency (exit 69 on failure)
prometheus mcp status          # start every enabled server and show its state
```

```console
$ prometheus mcp add docs --url https://docs.example.com/mcp --auth-secret DOCS_TOKEN
added MCP server 'docs'; its tools join sessions started from now (check it with: prometheus mcp test docs)
$ prometheus mcp list
docs  http  https://docs.example.com/mcp  trust untrusted
$ prometheus mcp test nope   # exits 66
error: no MCP server named 'nope' is configured
$ prometheus mcp remove docs
removed MCP server 'docs'; sessions started from now no longer get its tools
```

`enabled = false` keeps
a server configured without connecting it.

### Tools

The server's own tool create_issue registers as `mcp__github__create_issue` in
toolset `mcp-github`; characters outside `A-Z a-z 0-9 _` become `_`, and a
name longer than 64 characters keeps its first 55 and ends with `_` and 8
hex characters of its SHA-256. Configuring a server opts its toolset into
sessions (`tools.disabled_toolsets` still removes it).

`tools.include` is a whitelist (exact names or case-sensitive globs) that
wins over `tools.exclude`; an empty `include` registers nothing.

### When servers start, and when they fail

Servers start when a session needs them, never at process start, at most
`mcp.discovery_concurrency` (4) at once, each bounded by
`connect_timeout_secs` (`mcp.connect_timeout_secs`, 60 s). A session's tool
array is frozen when it starts: tools of a server that is down are left out
(`tools list --session <id>` shows them `unavailable:mcp_server_down: <reason>`).

A server that crashes mid-session does not take the session with it: its
tools answer `MCP server '<name>' unavailable: ...` and the model carries on.
It is restarted when next needed, after a backoff that doubles from 1 s to
60 s. Calls are bounded by `timeout_secs` (`mcp.call_timeout_secs`, 300 s);
after 3 consecutive failed calls (no answer in time, connection gone) the
server's circuit opens for 60 s: calls answer `MCP server '<name>' is
unreachable after 3 consecutive failures; retry in ~<n>s` without contacting
it, then one call is let through to probe it.

`/mcp` in a chat shows the servers; `/mcp reload` rebuilds them from
`config.toml` and their tools reach sessions started afterwards, or this
one at once with `/mcp reload --now` (which rebuilds the tool array, so the
prompt cache starts over). Safe mode (`--safe-mode`, `plugins.safe_mode`)
starts no MCP server.

### Sandbox and trust

A `stdio` server runs under the profile's OS sandbox like a plugin: the
capability baseline plus its `capabilities` table (`network` hosts,
`fs_read`/`fs_write` paths, absolute or under `$workspace`), reading its own
program's directory, starting children, and its scratch directory
`<profile>/cache/mcp/<name>` (also its `TMPDIR`). Its environment holds only
`PATH`, `HOME`, `LANG` (and `SYSTEMROOT`, `PATHEXT`, `USERPROFILE` on
Windows), `TMPDIR`, its `env` values and its `env_secrets`: no other secret,
such as a provider key, reaches it.

`trust = "untrusted"` (the default) asks for approval on every call of a
tool that does not declare `readOnlyHint`; in runs nobody can answer (one-shot
`-z`, cron) the approval mode of that run decides. `trust = "full"` treats the
server's tools like built-in ones.

## Tool search

Many MCP tools make for a large tool array. With `tools.tool_search.mode =
"auto"` (the default), when the schemas of the deferrable tools (MCP and
plugin tools, plus `tools.tool_search.defer`) exceed the smaller of
`threshold_pct` (5) percent of the model's context window and
`listing_max_tokens` (4,000) tokens, they leave the array and the model gets
three tools instead: `tool_search` (keyword queries, ranked; its description
lists the deferred tools within the listing budget), `tool_describe` (their
schemas) and `tool_call` (runs them). A `tool_call` runs the real tool
through the same checks, approvals and limits as a direct call, and only
tools of the session's own toolsets; anything else is `unknown tool: <name>`.
`mode = "on"` always defers, `"off"` never. `tools list --session <id>` shows
deferred tools as `deferred`.

## Serving Prometheus over MCP

```sh
prometheus mcp serve              # stdio (or --stdio)
prometheus mcp serve --http 127.0.0.1:8765
```

```console
$ prometheus mcp serve --http 0.0.0.0:8765   # exits 77
error: refusing to serve MCP on 0.0.0.0:8765: it is not a loopback address
```

The server is a non-interactive session of the selected profile. It offers
the toolsets of `mcp_server.expose_toolsets` (default `["skills"]`, which
offers only the read-only [`skills_list`](../reference/tools.md#skills_list) and
[`skill_view`](../reference/tools.md#skill_view)) as MCP tools, and
every skill as an MCP prompt and as resource `skill://<name>`; nothing
else. Every call runs under the profile's grants, sandbox and approval
policy, and a call that needs an approval is denied, since nobody can answer
it.

Over `--http` the server binds loopback addresses only, unless
`mcp_server.allow_remote = true`, and every request needs the bearer token
`MCP_SERVER_TOKEN` from the profile's secrets store. The first `--http` start
generates and stores one (`prometheus secrets get MCP_SERVER_TOKEN --reveal`
shows it); set your own with `prometheus secrets set MCP_SERVER_TOKEN`.
