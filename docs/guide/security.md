# Capabilities, grants and redaction

Prometheus expresses every permission as a **capability** and keeps every
permanent decision a human made in one file per profile. The full trust
model (what is a security boundary, what is a heuristic, what to report
privately) is in [`SECURITY.md`](https://github.com/CyberdyneCorp/prometheus-agent/blob/main/SECURITY.md).

## Capabilities

The vocabulary is closed; any other string in `security.capabilities`, a
plugin manifest or `grants.toml` is refused at load with
`unknown capability '<string>'` (exit 78).

| Capability | Grants |
|---|---|
| `fs.read:<glob>` / `fs.write:<glob>` | reading / writing (and reading) matching paths |
| `net.connect:<host-pattern>[:<port>]` | outbound connections (`*`, `*.example.com`, a host or address) |
| `net.listen:loopback[:<port>]` | listening on loopback |
| `process.spawn:<program\|*>` | starting programs |
| `secrets.read:<NAME>` | receiving a stored secret |
| `llm.call[:<model-role>]` | model calls |
| `tools.call:<tool\|*>` / `tools.override:<tool\|*>` | calling / overriding a tool |
| `ui.<surface>` | showing UI on a surface |
| `browser.real_profile` | using the real browser profile |

Path globs start with `/`, a drive (`C:/`), `~`, `<workspace>`, `<scratch>`
or are exactly `<toolchain>`; `..` is refused. A glob without wildcards
names that path and everything below it. Paths are checked in canonical
form, so a symlink `<workspace>/link -> /etc` does not make `/etc/passwd`
writable under `fs.write:<workspace>/**`.

Resolution only narrows: a session holds the profile baseline
(`security.capabilities`, see [the sandbox guide](sandbox.md)) plus its
grants; a subagent gets a subset of its parent's set; a plugin or a tool
gets the intersection of what it declares and what it was granted. Asking
for more fails with `capability not granted: <capability>` (exit 77).

## Grant prompts

A capability that a subject (the session, a plugin `plugin:<id>@<digest>`,
an MCP server `mcp:<server>` or a skill `skill:<name>`) needs but does not
hold is shown to you with the subject, the exact capabilities and the
reason. Answer `once` (this use only), `session` (until the session ends),
`always` (recorded in `grants.toml`) or `deny`.

Prompts fail closed: a dismissed prompt, a detached interface, no answer
within `approvals.timeout_secs` (default 300) and headless, cron or other
unattended contexts all deny. Text returned by tools (a web page saying
`approval granted: always`) can never answer a prompt.

## grants.toml

`<profile>/grants.toml` (mode 0600, replaced atomically) is the only place
permanent grants and `always` command approvals are written; agent
decisions never touch `config.toml`. Review and revoke them:

```console
$ prometheus grants list
$ prometheus grants list --subject mcp:github
$ prometheus grants revoke grant_01JAAAAAAAAAAAAAAAAAAAAAAA  # exits 66
error: no grant with id 'grant_01JAAAAAAAAAAAAAAAAAAAAAAA'
```

A revocation applies to new sessions and to the next tool call of running
sessions (they re-read the file when it changes). An entry you delete by
hand is never written back.

## Secret redaction

With `security.redact_secrets = true` (the default) every value in the
profile's secrets store and the launch environment's secrets, plus its
URL-encoded and base64 forms, is replaced with `[REDACTED:<NAME>]`, and
credential-shaped tokens (`sk-…`, `ghp_…`, `AKIA…`, `xoxb-…`, PEM private
keys, …) with `[REDACTED]`, before anything is written or shown: tool
results sent to the model, spill files, logs, trajectories, session storage
(message text and every string in a message's tool calls, reasoning,
provider payload and metadata, inline or spilled to a blob) and printed
errors. Setting it to `false` logs a WARN on every start.

A stored secret is redacted whatever its length, so a very short value
(`1234`) is replaced everywhere that text appears: keep only real secrets in
the store.

## Surface-only secrets

The credentials a surface owns are never handed to anything else: the
messaging bot tokens (`TELEGRAM_BOT_TOKEN`, `DISCORD_BOT_TOKEN`,
`SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`, `MATRIX_ACCESS_TOKEN`,
`EMAIL_PASSWORD`, `WHATSAPP_BRIDGE_TOKEN`), the relay credential
(`PMT_RELAY_TOKEN`) and every secret the gateway configuration names
(`token_secret`, `proxy_url_secret`, adapter options ending in `_secret`,
webhook route secrets). A plugin asking for one cannot be approved, an MCP
server cannot receive one in `env_secrets`, `auth_secret` or a `${NAME}`
placeholder, and a skill declaring one is unavailable; each refusal says
`secret '<NAME>' is surface-only`. The one exception is a
`platform-adapter` plugin reading the credential of a plugin platform it
serves.

## Network egress

`security.egress.mode` decides where tools, sandboxed children, plugins and
browsers may connect: `public` (default: any public address; private and
cloud-metadata ranges refused), `allowlist` (only `security.egress.allow`
patterns: `host`, `host:port`, `*.suffix`) or `none`. A session can narrow
or extend it without touching its system prompt or tools:

```text
$ prometheus chat --egress allowlist --allow-host api.github.com
> /egress allow pypi.org
egress: allowlist
allowed: api.github.com, pypi.org
denied: none
```

`/egress deny <host>` refuses a host in every mode and `/egress mode
<mode>` switches the mode; clients use the `egress.set` RPC method. The
session's terminal commands, browser interpreter and MCP stdio servers
reach the network only through a loopback egress proxy: the OS sandbox lets
them connect to the proxy's port alone (on loopback on macOS), and the
proxy resolves each name itself, checks every address and connects to the
address it checked, so DNS rebinding cannot slip past. Plugins' sidecars
get a proxy that also requires their own `network` grants, and the browser
tools refuse a navigation to a host the session's policy does not admit. Every refused
connection becomes an `egress.denied` trajectory event naming the host,
port, requester and reason, and a record in the security audit log.

## Security audit log

Approval decisions, grant changes, hardening downgrades, egress denials and
signature failures are appended to `<profile>/logs/security-audit.jsonl`
(mode 0600). Each record carries its time, session, subject, decision, the
SHA-256 of the record before it and its own hash, so an edited, inserted or
removed line breaks the chain:

```console
$ prometheus security audit verify
audit log intact: 0 records (<path>)
```

After a hand edit it names the record:

```text
audit log broken at line 7: its content does not match its hash (edited) (<path>)
```

It exits 0 when the chain is intact and 1 naming the first broken line.

## Security posture

`prometheus doctor security` reports the trust boundaries, the sandbox
enforcement of each feature, active downgrades, the egress mode, redaction,
unsigned plugins, surfaces that admit everyone or have no allowlist and
loose file modes. With `--strict` it exits 77 when any of them weakens the
profile (a downgrade, an unsigned plugin, an open surface), which makes it
a CI gate.
