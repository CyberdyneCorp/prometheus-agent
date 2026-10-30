# Connectors

## Purpose
Connectors are hosted app integrations (Gmail, Notion, Linear and similar) whose tools run on a managed tool gateway reached through a Nous sign-in, plus the agent-facing `manage_connections` tool that checks and authorizes those accounts and requests installation of catalog MCP servers. The key decision is that connector tools are never registered locally and never enter the frozen tool array: they are discovered, described and called only through the `tool_search` bridge, every state change needs the user (a card, a link the user opens, or the CLI), and the agent can never disconnect or delete an account. Config lives in the `connectors` section owned by this spec.

## ADDED Requirements

### Requirement: Connector availability
The system SHALL offer connectors only when `connectors.enabled` is true (default) and the profile has a Nous identity, SHALL never create a Nous identity without an explicit user action (a `manage_connections` card the user confirms, `prometheus connectors connect`, or the desktop Connectors page), and with `connectors.enabled = false`, or with no identity, or when the gateway does not serve connectors for the account, SHALL behave exactly as if the feature did not exist except that `tool_search` results MAY state once that hosted connectors are not set up and that `manage_connections` sets them up.
**Milestone:** M2

#### Scenario: Disabled feature is invisible
- **GIVEN** `connectors.enabled = false`
- **WHEN** the model calls `tool_search` with `["send an email"]`
- **THEN** no request SHALL be sent to the tool gateway and no result SHALL mention connectors

#### Scenario: No silent identity creation
- **GIVEN** a profile without a Nous identity
- **WHEN** `tool_search` runs
- **THEN** no identity SHALL be created and no request SHALL be sent to the gateway

### Requirement: Remote connector tools through the search bridge
The system SHALL name connector tools `connectors__<connector>__<tool>`, SHALL never place them in the frozen tool array, and SHALL make them reachable only through `tool_search`, `tool_describe` and `tool_call`: `tool_search` SHALL send each query to the gateway with a 30-second bound (a slow or unreachable gateway degrades to local results only, without an error), add the hits to the local catalog tagged `source = "connectors"` and rank local and connector documents in one pass so `limit` caps the group as a whole; when two remote names compose to the same `connectors__` name the system SHALL keep the conventional one, drop the other and log a WARN; `tool_describe` SHALL fetch connector schemas from the gateway; `tool_call` SHALL send each connector entry of a batch as its own gateway request in input order, retrying a name unknown under its conventional slug once under the literal slug, and splice the results back in the original order.
**Milestone:** M2

#### Scenario: Gateway slow
- **GIVEN** the gateway does not answer within 30 seconds
- **WHEN** `tool_search` runs
- **THEN** the result SHALL contain the local matches only and SHALL NOT be an error

#### Scenario: Connector tool absent from the tool array
- **WHEN** a session with connectors available is built
- **THEN** no tool whose name begins with `connectors__` SHALL appear in the serialized tool array

### Requirement: Connector calls run through the dispatch pipeline
Every connector `tool_call` entry SHALL pass the full `tool-system` dispatch pipeline under its real `connectors__` name (hooks, approvals, result budget), SHALL be treated as `effect = "external"` and non-idempotent, and SHALL carry provenance `{source: "connector:<connector>", trust: "untrusted"}`; a call needing an account the user has not linked SHALL return the error code `CONNECTION_REQUIRED` naming the connector and directing the model to `manage_connections` action `connect`.
**Milestone:** M2

#### Scenario: Unlinked account
- **GIVEN** no linked Gmail account
- **WHEN** the model calls `tool_call` with `connectors__gmail__send_email`
- **THEN** the result SHALL be `CONNECTION_REQUIRED` for `gmail`

### Requirement: Multiple accounts per connector
The system SHALL allow several accounts of one connector, each optionally named by an alias matching `^[a-z0-9][a-z0-9-]{0,31}$` (an unnamed account is shown by the app's own account label), with no default account; once a connector has two or more active accounts every tool of that connector SHALL take a `connector_alias` argument whose allowed values are the alias names (listed in the tool description as `Accounts: <a>, <b>.`), required when every active account is named and optional otherwise; a missing or unknown alias in that situation SHALL fail with an error listing the valid names; a result SHALL include `account` set to the alias it ran under when that account is named.
**Milestone:** M2

#### Scenario: Ambiguous account refused
- **GIVEN** Gmail accounts `home` and `work`
- **WHEN** the model calls a Gmail connector tool without `connector_alias`
- **THEN** the call SHALL fail listing `home` and `work`

### Requirement: manage_connections tool
The system SHALL provide `manage_connections` (toolset `connections`, in `core`, available only when `connectors.enabled` is true or a catalog MCP target exists) with arguments `action` in `status` (default), `display`, `connect`, `reconnect`, `rename`, `install`, `enable` or `authorize`, `connectors` (array of slugs or `{name, mcp?, alias?, to?}` objects; required except for `status` and `display`; `to` must match the alias pattern; at most one aliased target per call) and `force` (reconnect only); `connect`, `reconnect` and `rename` SHALL accept only hosted targets and `install`, `enable` and `authorize` only `mcp: true` targets, refusing the other kind with the call that would work; the tool SHALL have no action that disconnects, deletes or revokes an account and SHALL answer such requests by pointing the user to `prometheus connectors disconnect` or the portal.
**Milestone:** M2

#### Scenario: Wrong target kind
- **WHEN** the model calls `manage_connections` with `action = "install"` and `connectors = ["gmail"]`
- **THEN** the result SHALL be an error stating that `install` needs `{"name": "gmail", "mcp": true}` or that `gmail` is a hosted connector to `connect`

### Requirement: Connection flows require the user
On surfaces with an interactive card (`cli` in a TTY, `tui`, `desktop`) `connect`, `reconnect` and `display` SHALL show a card listing each target and SHALL block until every target is connected or skipped or `connectors.connect_timeout_secs` (default 300) passes, returning per target `connected`, `skipped` or `not_connected` and never a link; on surfaces without a card (gateway chats, single-query runs, cron) hosted actions SHALL return a `connect_url` per target for the user to open and SHALL NOT open it themselves, and `display` SHALL behave as `status`; MCP actions `install`, `enable` and `authorize` SHALL go through the `approval.request` flow and, where no approval channel exists, SHALL resolve as denied per `tool-system` › Approval channel fails closed; after a skip or timeout the tool description SHALL instruct the model not to re-ask on its own.
**Milestone:** M2

#### Scenario: Gateway chat gets a link
- **GIVEN** a Telegram session
- **WHEN** the model calls `manage_connections` with `action = "connect"` and `connectors = ["notion"]`
- **THEN** the result SHALL contain a `connect_url` for `notion` and no card SHALL be awaited

#### Scenario: Cron cannot install MCP servers
- **GIVEN** a cron run
- **WHEN** the model calls `manage_connections` with `action = "install"` for an MCP catalog entry
- **THEN** the result SHALL be `status = "denied"` and no config SHALL change

### Requirement: Reconnect and rename semantics
`reconnect` SHALL check each target and repair only accounts that are not connected (failed, expired or revoked), SHALL report an already connected account as such and do nothing unless `force = true` (account switch), and SHALL replace a broken account with a new one under the same alias while keeping the old one as retired (disabled, listed but folded away); `rename` SHALL set or change an account's alias (an unnamed account is matched by its label) and, when the portal does not support renaming, SHALL return `this Nous portal cannot rename connector accounts yet`; when the agent names a new account it SHALL tell the user the alias it chose.
**Milestone:** M2

#### Scenario: Reconnect keeps history
- **GIVEN** an expired Gmail account `work`
- **WHEN** `reconnect` completes for `work`
- **THEN** a new active account `work` SHALL exist and the old one SHALL be listed as retired

### Requirement: Connector account management CLI
The system SHALL provide `prometheus connectors list` (connectors with connected state), `accounts [<app>] [--all]` (`--all` includes retired accounts), `connect <app> [--alias <name>] [--reconnect [--force]]` (prints the link and waits until connected or `connectors.connect_timeout_secs`; Ctrl-C stops waiting and exits 130), `rename <app> <name-or-label> --to <new>` and `disconnect <app> [--alias <name>] [--yes]` (asks for confirmation unless `--yes`; non-interactive without `--yes` exits 4), each with `--json`; without a Nous sign-in the commands SHALL exit 78 with `connectors need a Nous sign-in: run prometheus auth login nous`, and an invalid alias SHALL exit 2.
**Milestone:** M2

#### Scenario: Disconnect needs confirmation
- **WHEN** `prometheus connectors disconnect gmail --alias work` runs without a TTY and without `--yes`
- **THEN** the command SHALL exit 4 and SHALL NOT remove the account

### Requirement: Connectors slash command
The system SHALL provide `/connectors` on the `cli`, `tui` and `desktop` surfaces listing connectors with their accounts, aliases and states (retired accounts folded), with inline actions to connect, reconnect and rename; it SHALL NOT offer disconnect except on `tui` and `desktop`, where disconnect requires an explicit confirmation.
**Milestone:** M3

#### Scenario: TUI list view
- **GIVEN** Gmail accounts `home` (connected) and `work` (expired)
- **WHEN** the user runs `/connectors` in the TUI
- **THEN** both accounts SHALL be listed with their states and a reconnect action SHALL be offered for `work`
