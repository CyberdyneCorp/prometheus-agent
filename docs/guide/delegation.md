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

With `delegation.independent_completions = true` (default false; new
sessions only) the model may give tasks a `group`: each ungrouped task's
result arrives as its own completion as soon as it finishes, and each
group's results arrive together once all its tasks finish. The handle lists
the completions as `units` (`<delegation_id>-1`, `-2`, … with their group
and task indexes). Grouping changes delivery only, never how or when the
children run. With the setting off a `group` is accepted and ignored.

In a messaging chat, `delegation.gateway_mode` decides whether a turn waits
for its children: `auto` (default) waits inside the [`delegate_task`](../reference/tools.md#delegate_task) call
where the platform's interim assistant messages are off (so the chat gets
one reply per turn) and dispatches in the background elsewhere; `inline`
always waits; `background` never does. A session that waits is not offered
the live-control actions, and `/stop` or an interrupting message stops its
children with their partial results. The mode is fixed when the session is
created. `prometheus chat -z` and piped runs always wait for their
children.

Every dispatch writes a human-readable live transcript per child under
`<profile>/cache/delegation/live/<delegation_id>/` (`task-1.log`, … and a
`manifest.json`), listed in the result as `live_transcripts`: each log
exists before the child's first model call and gains a timestamped line per
tool call, so `tail -f` follows a child. Directories older than 7 days are
pruned on later dispatches.

## Children's model requests

`delegation.request_overrides` sets request parameters on every child
model call, whatever its route (inherited, pinned or direct), winning over
the route's own values; its `extra_body` table is merged one level deep into
the request body:

```toml
[delegation.request_overrides]
temperature = 0.3

[delegation.request_overrides.extra_body.provider]
sort = "throughput"
```

A parameter the children's wire format does not take fails validation
(exit 78); when children inherit a route its format is known only per call,
so any parameter some native format takes is accepted.

`delegation.base_url` sends children to a direct endpoint instead of a
provider route (a task or agent that names its own model keeps it), with
`delegation.api_key_secret` (a secret name; it may be sent to that host
only), `delegation.wire_format` (default: inferred from the URL, as for a
custom provider) and `delegation.model` (or the parent's model id) as the
model there. Such children get no fallback chain.

## Background tasks and the agents monitor

`/background <prompt>` (alias `/bg`) starts an independent session of the
profile on this session's route, reasoning level, fallback chain and tool
selection, with none of its history, and answers at once with `Background
task #1 started: … (id bg_…)`. The task takes a slot of
`delegation.max_concurrent_children` (it waits for one when all are busy);
its answer arrives as a labelled notification (a chat message in a
messaging chat) and never enters this session. Its session is recorded with
source `background` and this session as its parent for lineage only: it is
the root of its own lineage (its usage and subagents are its own), `chat -c`
and `--resume latest` never pick it, and `prometheus chat --resume <id>`
continues it on this session's surface.

`/agents` (alias `/tasks`) lists what runs for this session with status,
elapsed time, model, tokens and cost: its subagents as a tree grouped by
parent (running and finished in the last hour), its background tasks and
its background processes. `/agents stop <id>` stops one subagent (its
siblings keep running), background task (`bg_…` or `#1`) or process;
`/agents steer <sa-id> <message>` steers a subagent; `/agents inspect <id>`
shows a returned subagent's or a task's history turn by turn; `/agents tail
<sa-id>` shows the end of a subagent's live transcript.

## Review

`/review [instructions]` starts a reviewer child in the background on the
delegation rail, with the session's grant and a normal child's tools. It
sees the session's last 10 user and assistant messages (tool output and
system messages left out, read from storage), the skills the session
loaded, and the instructions as its focus. It runs on `[delegation.review]`
`provider` and `model` (default: this session's route). `/review` answers
`Review started. Results will return here.`, and the findings re-enter the
session as a `delegation_completed` input labelled `Review: <focus>` (or
`Review recent work`), also when the review finishes while the daemon is
down and its completion is delivered after a restart.

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

[`execute_code`](../reference/tools.md#execute_code) (toolset `code_execution`) runs a Python script in the
session's own sandbox, with an empty environment (no secret reaches it). The script imports one stub per tool it
may use — the intersection of `code_execution.allowed_tools` with the
session's tools:

```python
from prometheus_tools import read_file, search_files
print(read_file(path="README.md"))
```

Every stub call is checked against the session's grant again and counts
against `code_execution.max_tool_calls`; the script stops after
`code_execution.timeout_secs`.

### The session kernel

On the local terminal backend each session keeps one Python kernel per
mode, interpreter, working directory and tool set, so imports and
variables persist from one call to the next. `reset: true` discards them
before the code runs. A call that hits the time limit or is interrupted
stops the kernel; its result carries `state_lost: true` and the next call
starts clean. A kernel unused for `code_execution.kernel_idle_timeout_secs`
(default 1800) stops, and beyond `code_execution.max_session_kernels`
(default 4) top-level sessions the least recently used one loses its
kernel (delegated children's kernels are not counted). Other backends run
each call as a one-shot script, and the result's `note` says so.

### Execution modes

`code_execution.mode` picks where and on which Python scripts run:

| Mode | Working directory | Python |
|---|---|---|
| `project` (default) | the session's | the virtual environment active in the session's terminal (`VIRTUAL_ENV`), else `.venv` in the working directory, when it runs and is 3.8 or newer; otherwise the managed runtime |
| `strict` | a fresh staging directory | the managed runtime |

Both modes use the same sandbox, scrubbed environment, tool allow-list and
limits. When a project environment exists but cannot be used (its
interpreter is missing, does not run or is too old), the script runs on
the managed runtime and the result's `note` says why.
