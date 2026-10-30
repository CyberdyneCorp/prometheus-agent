# ACP Adapter

## Purpose
The ACP adapter exposes Prometheus to editors (Zed, VS Code, JetBrains and other Agent Client Protocol clients) as `prometheus acp`, speaking newline-delimited JSON-RPC on stdio. The key decision is that the ACP process is a viewer of the profile's gateway daemon, never an execution owner: it translates ACP sessions, prompts, permissions and updates to and from the client protocol owned by the `workspace-architecture` capability (session-attachment semantics from `tui`), so editor sessions share storage, approvals, budgets and live attachment with every other surface.

## ADDED Requirements

### Requirement: Launch entry point and pre-server flags
The system SHALL expose the ACP server as `prometheus acp` (and `pmt acp`) and SHALL handle `--version`, `--check` and `--setup` before starting the stdio server and then exit; `--check` SHALL verify the profile resolves, the daemon is reachable or can be started, and a model is configured, printing `Prometheus ACP check OK` and exiting 0, or printing the first failing check and exiting 1.
**Milestone:** M3

#### Scenario: Check without a model
- **GIVEN** a profile with no default model
- **WHEN** the user runs `prometheus acp --check`
- **THEN** the system SHALL print `no default model configured for profile <name>; run 'prometheus setup'` and exit 1

### Requirement: Stdout carries only protocol frames
While the ACP server runs, stdout SHALL carry only ACP JSON-RPC frames; all logs SHALL go to stderr and to `<profile>/logs/acp.log` with secret redaction applied, and nothing else in the process (including subprocess output) SHALL inherit stdout.
**Milestone:** M3

#### Scenario: A component logs during a prompt
- **WHEN** any component logs while a prompt runs
- **THEN** the record SHALL appear on stderr with secrets redacted and stdout SHALL contain only valid JSON-RPC frames

### Requirement: Liveness probes are quiet
The server SHALL answer unknown methods with `-32601`, and for the probe names `ping`, `health` and `healthcheck` it SHALL do so without logging at WARN or above.
**Milestone:** M3

#### Scenario: Editor probe
- **WHEN** the client sends `ping`
- **THEN** the response SHALL be `-32601` and no warning SHALL be logged

### Requirement: Configuration comes from flags and the profile only
The ACP process SHALL resolve its ProfileScope from `--profile` (else the active profile) and read secrets from the profile's secrets store; launch overrides SHALL be accepted only as CLI flags (`--model`, `--provider`, `--yolo`, `--safe-mode`, `--ignore-user-config`), and no non-secret environment variable SHALL change ACP behaviour.
**Milestone:** M3

#### Scenario: Safe mode without a model
- **WHEN** an editor launches `prometheus acp --safe-mode` with no `--model` and `--provider`
- **THEN** `session/new` SHALL fail with `--safe-mode reads no profile default model: launch with --model <m> --provider <p>`

### Requirement: ACP process is a daemon viewer
The ACP process SHALL attach to the gateway daemon of its home by sending `rpc.hello` with `client: "acp"` and `capabilities.server_requests = true`, starting the daemon (as `prometheus daemon ensure`) when `daemon.autostart = true`; it SHALL NOT run turns, tools or MCP servers itself, and exiting SHALL only detach its sessions, never end them.
**Milestone:** M3

#### Scenario: Editor closes mid-turn
- **GIVEN** a running turn started from the editor
- **WHEN** the editor kills the ACP process
- **THEN** the turn SHALL continue in the daemon and SHALL be visible on `session/load`

### Requirement: Daemon connection is bound to the launch profile
The ACP process SHALL request a ticket granting only its launch profile, and SHALL refuse with `profile_mismatch` any daemon whose `rpc.hello` result does not list that profile in `profiles`.
**Milestone:** M3

#### Scenario: Wrong daemon
- **WHEN** the discovered daemon serves a different home that lacks profile `work`
- **THEN** the first session call SHALL fail with `profile_mismatch` and the connection SHALL be closed

### Requirement: Initialize handshake
`initialize` SHALL answer with the negotiated ACP protocol version, `agentInfo {name: "prometheus", version}` where `version` is the single build version also shown by `prometheus --version`, capabilities `loadSession: true`, prompt image support, session `list`, `fork` and `resume`, and the authentication methods; it SHALL record the client's advertised capabilities (`fs.readTextFile`, `fs.writeTextFile`, `terminal`, elicitation forms) for the connection.
**Milestone:** M3

#### Scenario: Version agrees across surfaces
- **WHEN** an editor calls `initialize`
- **THEN** `agentInfo.version` SHALL equal the version printed by `prometheus --version`

### Requirement: Authentication reuses profile credentials
The server SHALL always advertise the terminal auth method `prometheus-setup` (args `["setup"]`) and, when the profile's default provider has a credential, that provider id; `authenticate` SHALL succeed only for an advertised method whose credential now resolves.
**Milestone:** M3

#### Scenario: No credentials yet
- **GIVEN** a profile with no provider credential
- **WHEN** the editor calls `authenticate("prometheus-setup")`
- **THEN** the call SHALL fail and `initialize` SHALL list only `prometheus-setup`

### Requirement: New session on the daemon
`session/new` SHALL call `session.create` with `source: "acp"`, the editor cwd, the editor's MCP servers and the launch flags, and reply with `models`, `modes` (current `default`) and `configOptions`; it SHALL then send `available_commands_update` and a usage update.
**Milestone:** M3

#### Scenario: Successful creation
- **WHEN** the editor calls `session/new` with cwd `/work/app`
- **THEN** the daemon session SHALL have source `acp` and cwd `/work/app` and the reply SHALL carry mode `default`

### Requirement: Load and resume replay the logical session
`session/load` and `session/resume` SHALL map the editor-held id to the session's current lineage tip (following compression continuations), attach with `last_seen_seq` 0, replay the live transcript as `session/update` notifications before responding, and re-offer still-open approval and clarify requests; for an idle session whose cwd differs from the editor cwd, the cwd SHALL move to the editor's, and for a busy session the editor SHALL be told `Attached to the running session; editor cwd is not applied.`
**Milestone:** M3

#### Scenario: Reopen after compression
- **GIVEN** an editor-held id whose session was compressed into a continuation
- **WHEN** the editor calls `session/load` with that id
- **THEN** the adapter SHALL attach the continuation and address updates to the editor-held id

### Requirement: Editor MCP servers belong to the daemon session
Editor-provided `mcpServers` SHALL be passed to the daemon as part of the session's editor policy and run by the daemon under the session's capability grants; on load or resume a non-empty list SHALL replace the session's editor servers effective from the next turn, an empty list SHALL keep them, and a change SHALL be announced as `Editor MCP tools changed; the new tool list applies from the next turn.`
**Milestone:** M3

#### Scenario: Second editor with no servers
- **GIVEN** a session bound to editor MCP server `db`
- **WHEN** another editor loads it with `mcpServers: []`
- **THEN** server `db` SHALL remain bound

### Requirement: Session list is a paged read-only catalog
`session/list` SHALL return only ACP-created sessions of the launch profile with at least one message, one row per logical conversation (its newest lineage tip), optionally filtered by cwd, newest first, in pages of 50 with `cursor` being an opaque token; an invalid cursor SHALL return `-32602 invalid cursor`, never the first page.
**Milestone:** M3

#### Scenario: Forged cursor
- **WHEN** the editor passes cursor `abc`
- **THEN** the response SHALL be `-32602 invalid cursor`

### Requirement: Fork creates a daemon branch
`session/fork` SHALL call `session.branch`, move the child to the editor cwd when it differs, attach the child, and bind the child's editor MCP servers to the given list or else to the parent's.
**Milestone:** M3

#### Scenario: Fork without servers
- **WHEN** the editor forks a session that has editor MCP server `db` and passes no `mcpServers`
- **THEN** the child SHALL have server `db`

### Requirement: Prompt submission and terminal outcome
`session/prompt` SHALL submit staged content with `prompt.submit` and a fresh `input_id`, wait for that admission's terminal event, and return `stopReason: "end_turn"` on completion, `"cancelled"` on cancellation, `"max_tokens"` or `"max_turn_requests"` when the corresponding budget ended the turn, and a JSON-RPC error `admitted_turn_failed: <reason>` on failure; prompting a session this process has not attached SHALL return `not_found`.
**Milestone:** M3

#### Scenario: Turn fails
- **WHEN** the admitted turn ends with outcome `failed`
- **THEN** the prompt SHALL return an error rather than `end_turn`, and the next prompt SHALL be accepted

### Requirement: Cancel targets only this editor's admission
`session/cancel` SHALL cancel only the admission this ACP process submitted, interrupting the running turn only when that admission is the one running, and SHALL never interrupt a turn submitted from another surface; a cancel arriving before the admission id SHALL be applied when it arrives.
**Milestone:** M3

#### Scenario: TUI turn running
- **GIVEN** a turn submitted from the TUI is running in a shared session
- **WHEN** the editor sends `session/cancel`
- **THEN** the TUI's turn SHALL keep running

### Requirement: Exactly-once submit across lost acknowledgements
When `prompt.submit` fails before an acknowledgement, the adapter SHALL keep `(content fingerprint, input_id, staged content)` for that session (at most 64 sessions) and reuse the same `input_id` when the next prompt in that session has the same fingerprint, so the daemon returns the original admission; any other prompt or control SHALL retire the kept identity.
**Milestone:** M3

#### Scenario: Resend after a lost ack
- **WHEN** the editor resends the same prompt after a lost acknowledgement
- **THEN** exactly one admission SHALL exist and attached media SHALL be staged once

### Requirement: Daemon restart yields an unknown outcome, never a resend
On transport loss the adapter SHALL fail in-flight prompts with `Gateway disconnected; turn outcome is unknown. Load the session to check whether it completed; do not resend the work.`, reconnect on the next call, re-attach held sessions, and refuse new prompts in a session with an unknown admission until the user sends `/discard <admission_id>` or the admission's outcome becomes known on re-attach.
**Milestone:** M3

#### Scenario: Restart mid-turn
- **WHEN** the daemon restarts during a turn and the editor sends a new prompt
- **THEN** the adapter SHALL refuse it with `unknown_execution` naming `/discard <admission_id>` unless re-attach shows the admission's outcome

### Requirement: Slash commands are answered or refused, never failed
The adapter SHALL treat prompt text as a command only when its first token is a command registered for the `acp` surface; path-like or unknown `/words` SHALL be sent as prompt text; registered commands SHALL go to `command.dispatch`, whose `refused` reason SHALL be shown as an agent message without failing the prompt, and whose `send` message SHALL be submitted as the prompt; the advertised commands SHALL be exactly the registry's `acp`-surface commands, re-sent after every attach.
**Milestone:** M3

#### Scenario: Path at line start
- **WHEN** the user sends `/var/log/x is wrong`
- **THEN** the text SHALL be submitted to the model as a prompt

#### Scenario: Refused command
- **WHEN** the daemon refuses `/compress` because a turn is running
- **THEN** the editor SHALL see the refusal text and `session/prompt` SHALL return `end_turn`

### Requirement: Edit-approval modes
The adapter SHALL expose ACP modes `default` (ask before edits), `accept_edits` (auto-approve file edits inside the session workspace) and `dont_ask` (auto-approve edits for this session), mapped to the session's edit-approval policy; new sessions SHALL start at `default`, an unknown mode id SHALL return `-32602`, a mode change while a turn runs SHALL be refused with `Mode changes apply when the session is idle; try again after this turn.`, and a change from another surface SHALL be sent as `current_mode_update`.
**Milestone:** M3

#### Scenario: Mode changed in the desktop
- **WHEN** the desktop switches the shared session to `accept_edits`
- **THEN** the editor SHALL receive `current_mode_update` with `accept_edits`

### Requirement: Config options are validated
The adapter SHALL advertise `configOptions` for `edit_approval_policy`, `model` and `reasoning_effort`, and `set_config_option` SHALL return `-32602 unknown config option <id>` for any other id and `-32602` naming the allowed values for an invalid value, applying valid changes through daemon session mutations and returning the refreshed options.
**Milestone:** M3

#### Scenario: Unknown option id
- **WHEN** the editor calls `set_config_option` with id `temperature`
- **THEN** the response SHALL be `-32602 unknown config option temperature` and nothing SHALL change

### Requirement: Model switching through the daemon
`set_session_model` SHALL parse a provider-qualified model id, apply it as a session model mutation, and when a turn is running SHALL tell the editor it applies from the next turn; a refused switch SHALL be shown in the transcript with the daemon's reason.
**Milestone:** M3

#### Scenario: Switch during a turn
- **WHEN** the editor switches model while a turn runs
- **THEN** the editor SHALL see `Model change applies from the next turn.` and the running turn SHALL keep its model

### Requirement: Approvals and clarify bridge to editor cards with fail-closed semantics
Daemon approval requests SHALL become ACP permission requests offering only the choices the request allows (`allow_once`, `allow_session`, `allow_always`, `reject_once`, `reject_always`), with file edits shown as diffs, and clarify requests SHALL use elicitation forms when supported, else a permission card with one option per choice plus `Skip`; a dismissed or failed card SHALL withdraw this editor's answer without answering, so another driver may still answer, and when no other driver is attached the daemon SHALL settle the approval as denied immediately and the clarify as unanswered.
**Milestone:** M3

#### Scenario: Dismissed card, editor is the only driver
- **GIVEN** the editor is the only attached driver
- **WHEN** the user dismisses an approval card
- **THEN** the tool call SHALL be denied immediately rather than waiting for the deadline

#### Scenario: Dismissed card, desktop also attached
- **GIVEN** the desktop is attached to the same session as a driver
- **WHEN** the editor's card is dismissed
- **THEN** the approval SHALL stay pending and answerable from the desktop

### Requirement: Clarify is part of the ACP toolset
The `acp` toolset SHALL be the `coding` toolset including `clarify` and excluding messaging-send tools, resolved by the daemon like any surface (`toolsets.surfaces.acp` replaces it, `tools.disabled_toolsets` removes from it), and SHALL be fixed for the life of a session.
**Milestone:** M3

#### Scenario: Default ACP session
- **WHEN** an ACP session is created without overrides
- **THEN** its tools SHALL include `clarify` and SHALL NOT include messaging-send tools

### Requirement: Reply projection is exactly-once
The adapter SHALL stream assistant deltas as agent message chunks grouped by a per-segment `messageId`, start a new `messageId` at each tool call, show only the unseen remainder of the final message text, project reasoning deltas as thought chunks, and project the todo list as a `plan` update.
**Milestone:** M3

#### Scenario: Final text after streaming
- **GIVEN** 200 characters already streamed
- **WHEN** the final message of 230 characters arrives
- **THEN** only the last 30 characters SHALL be sent

### Requirement: Every tool call reaches a terminal status
Tool start and finish events SHALL be projected as ACP tool calls with kinds from a typed mapping (unmapped tools `other`) and the executor's error flag as the failure verdict; at turn end every call the admission started but never finished SHALL be marked `failed` with `This tool call ended without a result (blocked, denied, or interrupted).`
**Milestone:** M3

#### Scenario: Interrupted tool
- **WHEN** a turn is cancelled while a terminal command runs
- **THEN** the editor SHALL receive a `failed` update for that call

### Requirement: Prompt content conversion and media staging
The adapter SHALL keep text blocks as text, inline file `resource_link`s up to 512 KiB (larger files truncated with a note, binary files omitted with a note), stage images as blobs in the profile's content-addressed store with the type sniffed from bytes (PNG, JPEG, GIF, WebP only), accept at most 10 attachments per prompt (else `-32602 too many attachments (max 10)`), and fetch remote image URLs only through the egress-controlled fetcher, falling back to `[Image attached: <url>]` text.
**Milestone:** M3

#### Scenario: Unsupported image type
- **WHEN** the editor attaches an SVG
- **THEN** the prompt SHALL include `[Image omitted: unsupported image format]` and the text SHALL still run

### Requirement: Session metadata projection
The adapter SHALL send `session_info_update` when a non-empty title differs from the last one sent, usage updates from the session's usage events (cost from the usage ledger, including subagents), and `config_option_update` when model, provider, effort or mode changes from another surface.
**Milestone:** M3

#### Scenario: Same title republished
- **WHEN** the daemon republishes an unchanged title
- **THEN** no `session_info_update` SHALL be sent

### Requirement: Editor-native file access (upgrade)
When the client advertised `fs.readTextFile` and `fs.writeTextFile`, file read and edit tools in that ACP session SHALL read and write files under the editor cwd through the editor (`fs/read_text_file`, `fs/write_text_file`) so unsaved buffers are seen and edits land in open buffers; files outside the editor cwd and sessions without those capabilities SHALL use the daemon's own file tools, and a client fs error SHALL be returned to the model as the tool error.
**Milestone:** M3

#### Scenario: Unsaved buffer
- **GIVEN** `src/main.rs` has unsaved changes in the editor
- **WHEN** the agent reads `src/main.rs`
- **THEN** the content SHALL be the editor's buffer, not the file on disk

### Requirement: Editor terminal delegation (upgrade)
When the client advertised `terminal` support and `acp.use_editor_terminal = true` (default false), terminal tool calls of a local-backend ACP session SHALL run through the editor's `terminal/create` so the user sees them in the editor's terminal panel, with the same approval checks and output limits as daemon-run commands; because the editor terminal runs outside the Prometheus OS sandbox, a command SHALL use the editor terminal only when the session's capability set already permits unsandboxed local execution, and SHALL otherwise run sandboxed in the daemon.
**Milestone:** M3

#### Scenario: Command in the editor terminal
- **GIVEN** `acp.use_editor_terminal = true` and the client supports terminals
- **WHEN** the agent runs an approved `cargo test`
- **THEN** the command SHALL run in an editor terminal and its output SHALL be returned to the model within the tool's output budget

#### Scenario: Sandboxed session ignores the editor terminal
- **GIVEN** a session whose capability set requires OS sandboxing for local commands
- **WHEN** the agent runs a command
- **THEN** the command SHALL run in the daemon's sandbox, not in the editor terminal

### Requirement: Optional browser tools for ACP sessions
`prometheus acp --setup-browser [--yes]` SHALL install the managed Chromium runtime (asking before the download unless `--yes`, idempotent when already installed) and exit, `--setup` SHALL offer the same step after model selection, and the browser tools SHALL be part of an ACP session's toolset only when the session's resolved toolsets include `browser` and the Chromium runtime is available at session creation; otherwise they SHALL be absent rather than failing at call time.
**Milestone:** M3

#### Scenario: Runtime not installed
- **GIVEN** `toolsets.surfaces.acp` includes `browser` and Chromium is not installed
- **WHEN** an editor creates a session
- **THEN** the session's tools SHALL NOT include browser tools and `prometheus acp --check` SHALL report `browser runtime not installed; run 'prometheus acp --setup-browser'`

### Requirement: Unattended ACP clients and relay bridges
`prometheus acp --unattended` SHALL serve ACP clients that cannot show permission prompts (for example channel relay bridges such as `buzz-acp`): it SHALL never forward approval or clarify requests to the client, SHALL resolve approvals with `approvals.unattended_mode` (default `deny`) and the profile's grants, SHALL use the `toolsets.surfaces.acp_unattended` toolset (default the `acp` toolset without terminal and code-execution tools), and SHALL label its sessions with `source: "acp-unattended"`; a client permission answer received on an unattended connection SHALL be ignored and logged at WARN.
**Milestone:** M3

#### Scenario: Relay bridge auto-approves
- **GIVEN** a relay bridge connected through `prometheus acp --unattended` that answers every permission request with `allow_once`
- **WHEN** a channel message asks the agent to run `rm -rf build/`
- **THEN** no permission request SHALL be sent to the bridge and the command SHALL be blocked unless a grant or `approvals.unattended_mode = "approve"` allows it

#### Scenario: Default unattended toolset
- **WHEN** an unattended session is created without overrides
- **THEN** its tools SHALL NOT include the terminal or code-execution tools
