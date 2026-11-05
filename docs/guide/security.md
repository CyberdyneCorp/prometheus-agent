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

## PII redaction

With `privacy.redact_pii = true` (off by default; it applies from the next
session) every model request of the profile, main and auxiliary, plus its
logs and trajectories, has:

- phone numbers written with separators or in `+E.164` form replaced by
  `[REDACTED:phone]` (bare digit runs, dates, times, versions and IP
  addresses are left alone);
- email addresses replaced by `[REDACTED:email]`, except the owner's
  (`privacy.owner_emails`);
- messaging user ids (the `[name|id]` of group lines and the platform user
  of a gateway session) replaced by `user_<10 hex>`, a keyed hash that
  stays the same for one person across sessions, so the model can still
  tell people apart. The key is `<profile>/privacy.key` (0600).

The stored transcript keeps the original text, and replies are delivered
by the gateway's own routing, so they still reach the real person. This is
separate from secret redaction (`security.redact_secrets`).

```toml
[privacy]
redact_pii = true
owner_emails = ["me@example.com"]
```

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
`secret '<NAME>' is surface-only`. The one exception is the
`platform-adapter` plugin of a plugin platform reading that platform's own
credentials: the adapter plugin `acme` may hold the secrets of
`[gateway.platforms.acme]`, never those of another platform.

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
them connect to the proxy alone (on macOS its port on loopback; on Linux
they run in a network namespace of their own, see
[Sandbox](sandbox.md)), and the proxy resolves each name itself, checks
every address and connects to the address it checked, so DNS rebinding
cannot slip past. On Windows the child holds no network capability and
reaches the proxy through a relay process inside its own AppContainer
(see [Sandbox](sandbox.md)). Plugins' sidecars get a proxy that also requires their
own `network` grants. A local browser sends every connection (pages,
subresources, redirects, scripts, loopback included) through a proxy under
the session's policy, and the browser tools also refuse a navigation the
policy does not admit. WASM plugins' HTTP requests obey the calling
session's policy (the profile's outside a session) besides their `network`
grant; a private address needs `allow_private` or a grant naming the host,
and metadata endpoints are always refused. Every refused connection becomes
an `egress.denied` trajectory event naming the host, port, requester and
reason, and a record in the security audit log; a refusal of a child shared
by sessions (an MCP stdio server, a python sidecar) belongs to the session
whose call was in flight.

### Website blocklist

Hosts you never want reached are refused in every mode, by the web tools,
browser navigations and pages, and the children's egress proxy alike:

```toml
[security.egress]
deny = ["tracker.example", "ads*.example:443"]
deny_files = ["/etc/prometheus/blocked.txt"]
```

Patterns are `host` or `host:port` where `*` matches any run of
characters (`*.casino.example` covers every subdomain). A list file holds
one pattern per line with `#` comments and is re-read when it changes, so a
team can share one; a file that cannot be read is skipped with a warning.
A refused URL answers `Blocked: <host> is blocked by policy` and records
`egress.denied` with reason `denylist`. `/egress deny <host>` adds to the
list for one session; `/egress allow` never lifts a profile entry.

## Security guidance on written code

The first-party `security-guidance` plugin (off by default) reads what
[`write_file`](../reference/tools.md#write_file),
[`patch`](../reference/tools.md#patch) and skill writes put on disk and matches it against
33 heuristic rules: unsafe deserialization (`pickle.loads`, `yaml.load`,
Java `ObjectInputStream`...), command injection (`shell=True`,
`os.system`, `child_process.exec`...), XSS sinks (`innerHTML`,
`dangerouslySetInnerHTML`, `v-html`...), weak cryptography (MD5, SHA-1,
DES/RC4/ECB, disabled TLS verification), XXE, third-party scripts and
stylesheets without subresource integrity, GitHub Actions expression
injection and SQL built from strings.

```console
$ prometheus plugins enable security-guidance
```

With `plugins.security_guidance.mode = "warn"` (default) the write
happens and its result lists each matched rule id with its guidance (also
in the result's `security_guidance` metadata); with `block` the write is
refused and the guidance is the reason; `off` matches nothing. Findings
are heuristics: they flag code to review, they do not prove a
vulnerability.

## Supply-chain advisories

Each release embeds a catalog of known-compromised package versions, and
the update check (`prometheus update --check` and the background check)
refreshes it from `<origin>/channels/advisories.json`, verified against
the release keys like every release file; the kept copy is verified again
whenever it is read. At every start the catalog is matched against the
installed plugins, the python-bridge environments' pinned packages, MCP
servers pinned with `npx`/`bunx` (`pkg@1.2.3`) or `uvx` (`pkg==1.2.3`) and
the managed runtimes. Each match is one warning naming the advisory
(stderr on a terminal, the profile's and the gateway's logs):

```text
advisory PMT-ADV-2024-0001: @solana/web3.js 1.95.6 (mcp server wallet) is a known-compromised version; see prometheus doctor security, silence with prometheus doctor --ack PMT-ADV-2024-0001
```

`prometheus doctor security` lists every match with its summary and
remediation. `prometheus doctor --ack <id>` adds the id to
`security.acked_advisories`, which silences the startup warning only:
`doctor security` keeps showing it, marked `(acknowledged)`. An id the
catalog does not have exits 66.

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
