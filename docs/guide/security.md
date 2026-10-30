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
and printed errors. Setting it to `false` logs a WARN on every start.
