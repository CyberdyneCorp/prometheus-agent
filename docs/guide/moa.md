# Mixture of Agents

Mixture of Agents (MoA) lets one model call draw on several models. A
**preset** names some **reference** models (advisors) and one
**aggregator**. On a call routed to a preset, the references answer the
conversation first, without tools; the aggregator then reads their answers
as guidance and acts with the session's normal tools. Its reply (text and
tool calls) is the model's reply.

MoA is the virtual provider `moa` (alias `mixture-of-agents`): its models
are the presets. A preset is selected, cached, billed and used as a
fallback entry exactly like any other model.

## Presets

```toml
[moa]
default_preset = "default"   # what /moa runs
privacy_filter = ""           # "", "display" or "full"
save_traces = false
# trace_dir = "moa-traces"    # relative to the profile directory

[moa.presets.review]
reference_models = [
  { provider = "openai-codex", model = "gpt-5.5" },
  { provider = "openrouter", model = "deepseek/deepseek-v4-pro", reasoning_effort = "high" },
]
aggregator = { provider = "openrouter", model = "anthropic/claude-opus-4.8" }
# reference_temperature = 0.4   # unset: no temperature is sent
# aggregator_temperature = 0.2
fanout = "user_turn"           # or "per_iteration", "every_n:3"
enabled = true                 # false: the aggregator alone, as a plain model
```

Every profile has the built-in preset `default` (references
`openai-codex/gpt-5.5` and `openrouter/deepseek/deepseek-v4-pro`,
aggregator `openrouter/anthropic/claude-opus-4.8`) unless it defines its
own `[moa.presets.default]`.

A preset is refused when the config loads (exit 78, naming the key) if it
has no reference, if a slot names `moa` itself (presets do not nest) or an
unknown provider, if a `reasoning_effort` is not one of `none`, `minimal`,
`low`, `medium`, `high`, `xhigh`, `max`, `ultra`, or if a temperature is
outside 0.0-2.0. A slot without `reasoning_effort` uses the session's
reasoning level, clamped to its model.

## Selecting a preset

- `/model review --provider moa` or `/model moa/review` switches the
  session to the preset; a bare `/model review` does too when no alias is
  named `review` (an alias always wins; use `<provider>/<model>` for a
  model of the same name).
- `prometheus model set moa/review`, `chat --model moa/review` and the
  model pickers (the CLI, the TUI and messaging chats show a
  `moa (Mixture of Agents)` provider whose models are the presets) work
  the same way.
- `/moa <prompt>` runs exactly one turn with the whole argument as the
  prompt on `moa.default_preset`, then returns to the session's model,
  however that turn ends. The argument is never read as a preset name; a
  bare `/moa` prints its usage.
- `moa/<preset>` may be a `[[fallback.chain]]` entry. It is skipped when
  the preset is unknown or its aggregator has no usable credential.

An unknown preset fails with `model.unknown`, listing the preset names.

## What each call does

On each model call of a turn routed to an enabled preset:

1. When the cadence says so, every reference runs concurrently on a view
   of the conversation holding only user and assistant text: no system
   prompt, no tool schemas, no tool calls or results.
2. The aggregator receives the session's own system prompt, tools and
   history, plus one trailing private user message of labelled guidance
   with the references' answers. The guidance is never stored in the
   session, so each request's prefix is the previous request's, and the
   prompt cache keeps working across iterations. Anthropic, Bedrock and
   Gemini get it as a separate content block of the last user turn.
3. A reference that fails (no credential, rate limit, timeout...) becomes a
   one-line note in the guidance; the aggregator runs even when every
   reference failed. Only an aggregator failure fails the call, and then
   the session's fallback chain applies.

The cadence (`fanout`): `user_turn` (default) runs the references on the
first iteration of each user turn and reuses that guidance on the turn's
later iterations; `per_iteration` runs them every time; `every_n:N` (or
`{ mode = "every_n", n = N }`, N >= 2) on iterations 1 and every multiple
of N. Any other value logs one warning naming the preset and behaves as
`user_turn`.

When an aggregator endpoint refuses two consecutive user messages (HTTP
400, `Conversation roles must alternate`, as some vLLM chat templates do),
the request is retried once with the guidance merged into the user
message, and that endpoint gets merged requests for the rest of the
session.

## Billing

Every reference and aggregator call is its own usage-ledger row, billed to
its own provider and credential, with purpose `moa.reference` or
`moa.aggregator` and the preset as `purpose_detail`
(`prometheus usage --session <id> --by model` shows each slot). The
session's billing source is the aggregator's. When the aggregator's
provider is not `model.provider`, `prometheus moa list`,
`prometheus moa configure` and `/model` say so:

```text
Aggregator is on openrouter; the whole tool loop will be billed there, not to anthropic.
```

## Advisor output, privacy and traces

The references' answers are shown after the turn as `advisor <model>:`
notices. `moa.privacy_filter = "display"` redacts known secrets, email
addresses and delimited phone numbers (`(555) 123-4567`, `555-123-4567`)
in those notices and in traces while the aggregator still reads the raw
text; `"full"` also redacts the guidance the aggregator reads. Bare digit
runs, line numbers, timestamps, git SHAs and IP addresses are never
redacted.

With `moa.save_traces = true`, each MoA call appends one JSONL record to
`<profile>/moa-traces/<session_id>.jsonl` (or under `moa.trace_dir`): every
slot's request summary, output, usage and error, after the privacy filter
and known-secret redaction. Session exports (`sessions export`, `/save`,
JSONL) include them as `moa_trace` records, and they are deleted with
their session's trajectory by retention.

## Managing presets

- `prometheus moa list [--json]` lists the presets, marking the aggregator
  as the acting, billed model and the references with their cadence.
- `prometheus moa configure [<name>]` creates or updates a preset
  interactively (default: `moa.default_preset`), choosing each slot with
  the model picker and validating its credential.
- `prometheus moa delete <name>` deletes a configured preset; deleting
  the one `moa.default_preset` names exits 1 until another default is set.
- The daemon serves `moa.presets.list`, `moa.presets.put` and
  `moa.presets.delete` for preset editors.

On a fresh profile only the built-in preset exists, and it is the default:

```console
$ prometheus moa list
default (default)
aggregator: openrouter/anthropic/claude-opus-4.8 (acts with the tools and is billed for the tool loop)
references: openai-codex/gpt-5.5, openrouter/deepseek/deepseek-v4-pro (advise on user_turn)
$ prometheus moa delete default  # exits 1
<>preset 'default' is moa.default_preset; set another default first
```

Preset edits reach model calls of sessions started afterwards; a call
already in flight is never altered. Selecting, switching to or away from a
preset never changes the system prompt, tool schemas or earlier messages.
