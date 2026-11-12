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
added MCP server 'docs'; its tools join sessions started from now
$ prometheus mcp list
docs  http  https://docs.example.com/mcp  trust untrusted
$ prometheus mcp test nope   # exits 66
error: no MCP server named 'nope' is configured
$ prometheus mcp remove docs
removed MCP server 'docs'; sessions started from now no longer get its tools
```

`mcp add` probes the new server and, on a terminal, offers its tool
checklist (`prometheus mcp tools <name>` re-opens it; `tools.include` is
written only when not every tool is selected). `--preset codex` fills in
`command = "codex"`, `args = ["mcp-server"]` (explicit flags win).
`prometheus mcp` alone, on a terminal, is a picker over the configured
servers. `enabled = false` keeps a server configured without connecting it.

`${NAME}` in a server's command, arguments, `env` values, URL or `headers`
is filled at each start from the context variables `${userHome}`,
`${workspaceFolder}`, `${workspaceFolderBasename}` and `${pathSeparator}`,
else from the secrets store (never from the environment).

### Signing in (OAuth)

An `http` server with `auth = "oauth"` signs in with OAuth 2.1 and PKCE:

```sh
prometheus mcp add linear --url https://mcp.linear.app/mcp --auth oauth
prometheus mcp login linear          # --flow device for a device code
prometheus mcp reauth linear         # forget the sign-in and sign in again
```

The login discovers the authorization server (protected resource metadata,
then RFC 8414 metadata whose issuer must match), identifies with
`oauth.client_id` when set (a confidential client also sets
`oauth.client_secret_name`: its secret authenticates every token and
device-authorization request with HTTP basic), else with
Prometheus' client metadata document where the server accepts one, else by
registering a client dynamically. A server that does none of these needs a
pre-registered client: the login exits 78 saying so. It prints the
authorization URL (and opens a browser only from a foreground terminal that
is not an SSH session); paste the URL the browser was redirected to when it
cannot reach `127.0.0.1` (`oauth.redirect_host = "localhost"`,
`redirect_port`, `redirect_uri` adjust the redirect). The redirect's `state`
must match and its `iss`, when present or promised, must name the issuer.

Tokens live only in the secrets store (`MCP_OAUTH_<SERVER>_ACCESS`,
`_REFRESH`); the access token is refreshed under the profile's refresh lock
shortly before it expires. A refresh token that is refused (or an
authorization server that changed) parks the server: its tools are
unavailable, nothing opens a browser, and `prometheus mcp status` and
`prometheus doctor` show `needs re-authorization: prometheus mcp login
<server>`.

### Client certificates and identity headers

```toml
[mcp.servers.team_api]
transport = "http"
url = "https://api.internal/mcp"
client_cert = "/etc/pki/me.pem"           # certificate (and key, unless client_key)
client_key = "/etc/pki/me.key"
client_key_password_secret = "ME_KEY_PASS" # for an encrypted PKCS#8 key
identity_header = { name = "X-User-Id", value_from = "profile" }   # or "static" with value
[mcp.servers.team_api.headers]
X-Team = "ops"
```

A missing certificate file fails the start with `MCP server '<name>':
client certificate not found: <path>`. The identity header carries the
active profile's name (or the static `value`) and never replaces a
same-named `headers` entry; an invalid block, or one on a `stdio` server, is
warned about and ignored.

### The catalog

```sh
prometheus mcp catalog                   # MCP servers of the plugin catalog
prometheus mcp install linear --field TEAM=ops
```

Catalog entries are signed with the plugin catalog. An install writes the
server disabled (enable it with `prometheus config set
mcp.servers.<name>.enabled true`), asks the entry's setup fields (secret ones
hidden and stored in the secrets store, kept as `${NAME}` placeholders),
checks out a pinned git `source` under `<profile>/mcp/<name>` (`${installDir}`)
and runs its `bootstrap` commands in the sandbox, with only `PATH`, `HOME`,
`LANG` (and on Windows `SYSTEMROOT`, `PATHEXT`, `USERPROFILE`) from your
environment, so no exported API key reaches them, and with network access
only through the egress proxy under `security.egress`. An entry declaring
`tools.default_excluded` writes `tools.exclude` without a checklist;
otherwise the checklist pre-checks your earlier selection, else
`tools.default_enabled`, else everything. An install succeeds even when the
probe fails, and is never updated behind your back. An entry needing a newer
Prometheus is listed with `requires a newer Prometheus` and not installed.

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

Session start waits for servers still connecting at most
`mcp.discovery_wait_ms` (1,500) interactively and
`mcp.single_query_discovery_wait_ms` (15,000) for one-shot runs; a server
that misses the window joins later sessions. A new interactive session
does not hold its prompt for that wait: it shows the prompt at once and
waits (what is left of the window) before its first request, so a slow
server never delays the prompt; `/mcp reload --now` brings a late server's
tools into the running session. A `lazy = true` server
registers its tools from the cached tool list and starts on its first call.
A `stdio` server with `idle_timeout_secs` or `max_lifetime_secs` is stopped
after that idle time or age and restarted on its next call; its tools stay.

The daemon follows `mcp.servers` edits within a minute
(`mcp.auto_reload_on_config_change`, on by default): removed and disabled
servers stop, unchanged ones keep their connection, added ones start (a
failed first start is retried after 30 s, doubling to 10 minutes). Running
sessions keep their tools.

When a server announces that its tools changed, its tool list is fetched
again (a burst of announcements costs at most two fetches); running
sessions keep their tool array and show a one-line notice, and a call to a
tool the server dropped answers `tool '<name>' is no longer offered by MCP
server '<server>'`. A session picks the new list up at its next compaction
(the prompt cache restarts there anyway), keeping the toolsets it runs with
and the tools it disabled, or at once with `/mcp reload --now`. A server advertising resources or prompts also gets
`mcp__<server>__list_resources`/`read_resource` or
`mcp__<server>__list_prompts`/`get_prompt` (under the same filter).

Results are cleaned before the model reads them: invisible Unicode TAG
characters are removed (from tool descriptions, resources and prompts too,
keeping flag emoji), and a result's `_meta` reaches the model as `meta`
without protocol-reserved keys. Images and audio are stored in the
profile's blob store and reported as `MEDIA:<path> (<mime>)` (an image is
also attached when the model takes images); a block over 50 MiB fails the
call naming it.

A server's bare name works as its toolset name (`tools.enabled_toolsets =
["core", "github"]`) unless a built-in toolset has that name; `prometheus
doctor` warns then to write `mcp-<server>`.

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

### What servers may ask

**Sampling** (`sampling/createMessage`) is answered only for `trust = "full"`
servers unless `[mcp.servers.<name>.sampling] enabled` says otherwise, on the
session's route (or `model`, which must be in `allowed_models` when that list
is set), capped at `max_tokens_cap` (4,096) tokens, within `timeout_secs`
(30), `max_rpm` (10 per sliding minute) and `max_tool_rounds` (5); every call
is charged to the session as `mcp:<server>` and counts toward its budget. A
request over a limit gets an error naming it and no model call is made.

**Elicitation** (`elicitation/create`, form mode; `elicitation.enabled`,
default on) asks the person through the session's clarify prompt (answer
once or decline), whatever the approval mode or YOLO says; it is declined
after `elicitation.timeout_secs` (300), at once where no one can answer
(cron, one-shot runs), and always for URL-mode requests.

**Costs**: `tool_costs` (a number, or `{ credits = [min, max], note }`, per
tool), `spend_limit` (per conversation; 0 is none) and `cost_unit`
(`credits`). The approval prompt of a costed tool states its cost and the
conversation's running total (which follows the conversation across
compression and branches); a call that would cross `spend_limit` asks a
one-time approval even on a trusted server or under YOLO, and where no one
can answer it is refused telling the model not to retry. A malformed
`tool_costs` entry skips the server with an error naming it (`mcp status`
shows `error`).

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
