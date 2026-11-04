# Connectors

Connectors are hosted app integrations (Gmail, Notion, Linear and similar)
whose tools run on the Nous connector gateway. They use the profile's Nous
Portal sign-in; nothing else needs to be installed.

## Availability

Connectors are offered when `connectors.enabled` is true (the default) and
the profile is signed in to Nous (`prometheus auth login nous`). Prometheus
never creates a Nous identity on its own: without a sign-in, or with
`connectors.enabled = false`, sessions behave as if connectors did not
exist and no request reaches the gateway.

```toml
[connectors]
enabled = true
connect_timeout_secs = 300      # how long a card or `connectors connect` waits
gateway_url = "https://connector-gateway.nousresearch.com"
```

`gateway_url` points at a self-hosted gateway; it must be `https` (plain
`http` only on loopback), as the sign-in travels with every request.

## How the model uses them

Connector tools are never part of the tool array the model gets with every
call (that would break prompt caching and cost tokens). They are found,
described and run through the tool-search bridge:

- `tool_search` sends the queries to the gateway (bounded to 30 seconds; a
  slow or unreachable gateway only leaves out its results) and ranks its
  hits together with the session's local deferred tools, so `limit` caps
  the whole list. Connector tools are named
  `connectors__<connector>__<tool>`, e.g. `connectors__gmail__send_email`.
- `tool_describe` fetches a connector tool's schema from the gateway.
- `tool_call` runs connector tools one request each, in order. Every call
  goes through the whole dispatch pipeline under its real name (hooks,
  approvals, result budget), counts as an external, non-idempotent action
  and its result carries the provenance
  `{"source": "connector:<connector>", "trust": "untrusted"}`.

A call needing an account that is not linked fails with
`CONNECTION_REQUIRED`, naming the connector; the model then offers to
connect it with [`manage_connections`](../reference/tools.md#manage_connections).

## Several accounts

A connector may have several accounts, each optionally named with an alias
(`^[a-z0-9][a-z0-9-]{0,31}$`); there is no default account. Once a
connector has two or more active accounts, its tools take a
`connector_alias` argument (`Accounts: home, work.` in their description),
required when every account is named. A missing or unknown alias fails
with the valid names; a result names the account it ran under.

## manage_connections

The [`manage_connections`](../reference/tools.md#manage_connections) tool (toolset `connections`, in `core`) checks and
sets up connections:

| Action | Targets | What it does |
|---|---|---|
| `status` (default) | optional | connectors, their accounts and states |
| `display` | optional | a card on the CLI, TUI and desktop; `status` elsewhere |
| `connect` | hosted | starts an authorization |
| `reconnect` | hosted | repairs accounts that are not connected; `force: true` replaces a connected one |
| `rename` | hosted (`to`) | sets an account's alias |
| `install`, `enable`, `authorize` | `{"name": ..., "mcp": true}` | catalog MCP servers, after the user approves |

Every change needs the user:

- On a terminal, the TUI and the desktop, `connect`, `reconnect` and
  `display` show a card with each link and wait (up to
  `connectors.connect_timeout_secs`) until each target is connected or
  skipped. The model only learns `connected`, `skipped` or `not_connected`,
  never the link, and is told not to ask again after a skip or timeout.
- In messaging chats, one-shot runs and cron jobs the result carries a
  `connect_url` per target for the user to open; the model is told never to
  open it itself.
- `install`, `enable` and `authorize` always ask for approval; where no one
  can approve (cron, unattended runs) they are denied and nothing changes.

A reconnect keeps history: the broken account is retired (listed with
`--all`, never used) and a new account takes its alias. The tool cannot
disconnect, delete or revoke accounts; it points the user to the CLI or the
Nous portal.

## Command line

With a Nous sign-in and accounts on the gateway, a session looks like this:

```text
$ prometheus connectors list
gmail  connected
notion  not connected
$ prometheus connectors accounts gmail --all
gmail  work  me@work.example  active
gmail  work  me@work.example  retired
$ prometheus connectors connect notion --alias team
Open this link to connect notion:
  https://...
Waiting for the account (Ctrl-C stops waiting)...
notion (account 'team'): connected
$ prometheus connectors connect gmail --alias work --reconnect [--force]
$ prometheus connectors rename gmail me@home.example --to home
$ prometheus connectors disconnect gmail --alias work [--yes]
```

Every command takes `--json`. Without a Nous sign-in they exit 78, and an
invalid alias exits 2 before anything else is checked:

```console
$ prometheus connectors list # exits 78
error: connectors need a Nous sign-in: run prometheus auth login nous
$ prometheus connectors connect gmail --alias Work # exits 2
```

Ctrl-C while `connect` waits exits 130 (the link stays valid for a while),
and `disconnect` asks for confirmation and, without a terminal and without
`--yes`, exits 4 and removes nothing. A portal that
cannot rename accounts yet answers `this Nous portal cannot rename
connector accounts yet`.

The `/connectors` slash command arrives later (M3).
