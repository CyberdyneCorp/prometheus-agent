# Subagents and programmatic tool calls

The agent can hand work to **subagents** with the [`delegate_task`](../reference/tools.md#delegate_task) tool
(toolset `delegation`). Each task runs in a child agent of its own: a new
`subagent` session with its own iteration budget and terminal, no context
files, no way to ask you questions, and a read-only copy of the memory the
parent started with. The child returns a structured result (status,
summary, files written, usage and cost) instead of its transcript.

## What a child may do

A child's grant is always a subset of its parent's: the tools of the
toolsets a task asks for (composite toolsets such as `coding` are expanded
first), intersected with the parent's tools, and the parent's filesystem,
network and secret capabilities. Asking for anything the parent lacks
refuses the call with `delegation.grant_broadening`, naming every missing
item. Children never get `clarify`, `memory`, [`send_message`](../reference/tools.md#send_message), `cronjob` or
kanban tools. With the default `delegation.max_spawn_depth = 1` children
are leaves: they cannot delegate further.

Approvals a child needs are denied (`delegation.child_approval = "deny"`)
or, with `"escalate"`, shown on the root session tagged with the child's
`sa-…` id and denied when nobody answers within
`delegation.escalation_timeout_secs`. No setting approves a child's
dangerous action on its own, and a parent's `--yolo` or one-shot
`approvals.single_query_mode = "approve"` does not reach its children.

## Named agents

A Markdown file `<profile>/agents/<name>.md` with front matter `name`
(equal to the file name) and `description` (at most 600 characters)
defines an agent; optional `toolsets`, `model`, `max_iterations`,
`output_schema` and `background` shape its children, and the body becomes
their system prompt. A session uses the definitions that existed when it
started.

## Where results go

From an interactive client of the daemon (TUI, desktop, an attached line
chat) or a messaging chat, a spawn returns at once with a
`dg-…` delegation id and the children run in the background; their results
arrive later as a `delegation_completed` input that starts a new turn.
Nested spawns, `chat -z` runs, scheduled jobs and other sessions that cannot
take a later message wait for their children instead, and say why in the
result's `note`. A finished background delegation is recorded in the
profile's store first, so a restart of the daemon delivers it exactly once
when the session is next loaded; children still running at a restart end
`interrupted`.

At most `delegation.max_concurrent_children` children run at once per
profile; more wait in a fair queue (up to `delegation.max_queued`). A child
that makes no progress for 450 seconds between steps, or 1200 seconds in one
tool call, is stopped as `stalled`.

## Control

`/delegation` shows the state and this conversation's children;
`/delegation pause` and `/delegation resume` stop and restart new spawns for
the profile; `/delegation router <name>` changes the router for new
sessions (add `--now` to rebuild this session's tools). `/stop all` stops
every subagent of the session. The model itself can `list`, `status`,
`steer`, `stop` and `wait` on children of its own conversation. Every
surface shows the live spawn tree: a line when a child starts and ends, and
its current step and activity as it changes (at most every 3 seconds per
child).

## Routers

`delegation.router` (default `default`) names the router that places each
task: the built-in one keeps every child in this profile on the normal model
choice; a `delegation-router` plugin may add typed task fields (checked
against its schemas) and send a task to another model, another profile or a
remote backend, or refuse one task (it ends `refused` while the others run).
A router no enabled plugin provides fails every spawn with
`delegation.router_unavailable`; nothing falls back to `default`. A child
routed to another profile runs entirely under that profile's configuration,
secrets and sandbox policy (never the launching profile's secrets), its
grant intersected with that profile's grants; its session is still recorded
here. The router, the agent definitions and `[delegation]` are frozen when a
session starts.

## Plugins

A tool plugin granted `subagents` can launch and follow subagents of the
session that calls it through the `subagents` host interface (see the
[plugin authoring guide](plugin-authoring.md)): the same checks, grant and
limits as a spawn by the model, with the result returned to the plugin
instead of the conversation.

## Processes a child leaves

When a child ends, its background processes and code kernels are stopped
(those still running after 10 seconds are listed as `orphaned_processes`
on its result), except a process it handed to its parent with
`process(action="handoff")`: that one keeps running, owned by the parent,
and its exit notice goes to the parent.

The JSON Schema of a child's result:

```console
$ prometheus schema subagent-result
<...>"title": "SubagentResult"<...>
```

## Programmatic tool calls

[`execute_code`](../reference/tools.md#execute_code) (toolset `code_execution`) runs a Python script on the
managed Python runtime in the session's own sandbox, with an empty
environment (no secret reaches it). The script imports one stub per tool it
may use — the intersection of `code_execution.allowed_tools` with the
session's tools:

```python
from prometheus_tools import read_file, search_files
print(read_file(path="README.md"))
```

Every stub call is checked against the session's grant again and counts
against `code_execution.max_tool_calls`; the script stops after
`code_execution.timeout_secs`.
