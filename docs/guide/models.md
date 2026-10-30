# Models and providers

A **provider** is where model calls go: a hosted API (Anthropic, OpenAI,
OpenRouter...), a local inference server (Ollama, LM Studio, vLLM) or a
custom OpenAI-compatible endpoint. Each profile has its own provider
registry: the built-in providers, the `[providers.<id>]` blocks of its
`config.toml` and the provider plugins enabled in that profile. A name
that is not a provider id or alias in that registry is an error, never a
silent fallback to another provider.

Every command below is run by the documentation tests against a fresh home,
in order. Lines starting with `$` are commands; the lines after a command
are (part of) its output, with `<...>` standing for text that varies.

## How a route is chosen

The provider and model of a new session come from, in order: the
`--provider` / `--model` flags, a session override (`/model --session`),
`model.provider` / `model.default` in `config.toml`, and otherwise `auto`.
Environment variables never choose a provider: an exported
`PMT_INFERENCE_PROVIDER` is ignored with a warning. Only secrets come from
the environment.

`auto` picks the configured `model.provider`, else the first provider of
`model.auto_order` (default `anthropic`, `openai`, `openrouter`, `google`,
`nous`) whose secret is stored, else a provider you are signed in to, else
a configured local endpoint that answers. With nothing configured:

```console
$ prometheus model explain  # exits 78
<>Prometheus is not connected to any model provider. Run 'prometheus model' or 'prometheus auth add <provider>'.
```

`prometheus model explain [--provider ID] [--model ID] [--json]` prints
each resolution step (requested provider, provider lookup, the
`enabled` guard, endpoint, wire format, credential, model, context window
check) without calling the model. Credentials are shown by source
(`secret:<NAME>`, `pool:<provider>#<n>`, `subscription:<provider>/<label>`, `none`),
never by value (setting up keys, pools and subscription sign-ins is in
[Provider credentials and sign-ins](providers.md)):

```console
$ echo -n "$TOKEN" | prometheus secrets set OPENROUTER_API_KEY
$ prometheus model explain --provider or
provider ok 'or' -> 'openrouter'
credential ok secret:OPENROUTER_API_KEY
route: openrouter/anthropic/claude-sonnet-4.5 via chat_completions at https://openrouter.ai/api/v1 (credential secret:OPENROUTER_API_KEY, billing api_key:openrouter)
$ prometheus model explain --provider opnrouter  # exits 78
<>Unknown provider 'opnrouter'. Did you mean 'openrouter'?
```

A disabled provider (`providers.<id>.enabled = false`) fails before
anything else is resolved, with `provider.disabled`.

## Credentials stay on their hosts

A provider secret is sent only to the hosts it is bound to: the host of
the provider's default endpoint (its `key_hosts`), plus any host you add
with `providers.<id>.key_hosts` (exact names, or `*.example.com` for
subdomains). Pointing a provider at another host does not carry its key
along, so `OPENAI_API_KEY` never reaches an aggregator and
`DEEPSEEK_API_KEY` never reaches `api.deepseek.com.attacker.test`; such a
route fails with `provider.missing_credential` (exit 78).

Local servers (`ollama`, `lmstudio`, `vllm`, `sglang`) need no key. When
`providers.<id>.base_url` is not set, Ollama and LM Studio are tried at
their default loopback address; if nothing answers there, or a server
without a default (vLLM, SGLang) has no `base_url`, the route fails with
`provider.missing_base_url`. A local provider never falls back to a remote
one unless that provider is an entry of your fallback chain.

## Custom providers

A `[providers.<id>]` block whose id is not built in defines a custom
provider. It needs a `base_url`; `wire_format` is `chat_completions`
(default), `responses` or `anthropic_messages`; `key_secret` names its
secret. Plain `http://` is accepted only for loopback and private-network
hosts unless the block sets `allow_insecure_http = true`:

```console
$ prometheus config set providers.myproxy.base_url http://llm.example.com/v1  # exits 78
<>providers.myproxy.base_url: plain HTTP to public host 'llm.example.com' is refused; use https:// or set providers.myproxy.allow_insecure_http = true
$ prometheus config set providers.ollama.base_url http://127.0.0.1:11434/v1
```

The wire format of a route is the block's `wire_format`, else a host rule
(a base URL path ending in `/anthropic` or `/anthropic/v1` speaks
Anthropic Messages), else the provider's own format. A provider whose own
format is Anthropic Messages but whose `base_url` points at another host
(an OpenAI-compatible relay) speaks Chat Completions.

## Retries and the fallback chain

A failed call is classified as one of `auth`, `entitlement`, `billing`,
`rate_limit`, `overloaded`, `server_error`, `timeout`, `network`,
`context_overflow`, `content_filter`, `invalid_request` or `config`.
`rate_limit`, `overloaded`, `server_error`, `timeout` and `network` are
retried on the same route up to `providers.<id>.max_retries` times
(default 2) with jittered exponential backoff from 1 second (at most 30),
honouring a provider `Retry-After` of up to 60 seconds. Each request has
`providers.<id>.request_timeout_secs` (600) and a stream that stays silent
for `stream_idle_timeout_secs` (120) is aborted as a `timeout`.

When retries are spent on a failure of those classes, or of `auth`,
`entitlement` or `billing`, the next usable entry of the fallback chain
takes over for the rest of the turn:

```toml
[[fallback.chain]]
provider = "openrouter"
model = "anthropic/claude-sonnet-4.5"

[[fallback.chain]]
provider = "ollama"
model = "qwen3"
```

Entries that are disabled, have no usable credential, whose model lacks a
capability the request needs (tools, images, a large enough window) or
that are the route that just failed are skipped; duplicate entries keep
the first. Every switch is reported as a `provider.fallback` notice. The
next turn starts on the primary route again, unless the primary was left
for `rate_limit` or `billing`: then it rests until the provider's reset
time, or 1, 2, 4... minutes (at most 4 hours) for consecutive failovers.
`invalid_request` and `config` failures are never retried or rerouted.

Switching routes never edits the system prompt, the tool list or earlier
messages, so going back to a route replays exactly the prompt prefix it
had cached.

## Prompt caching

Requests carry prompt-cache breakpoints: Anthropic Messages routes get
`cache_control` markers (five minutes, or one hour with
`prompt_cache.ttl = "1h"`), and OpenAI-style providers that support it get
a prompt cache key that stays the same for the whole session.
`prompt_cache.ttl = "off"` sends neither. Cache read and write tokens are
recorded for every call in the usage ledger (see
[Usage, costs and budgets](usage-and-budgets.md)), with the call's
`billing_source`: `api_key:<provider>`, `subscription:<provider>`, or
`local` for a keyless local server.

## Model metadata and the minimum context window

A model's output cap, capabilities and prices come from
`[models."<provider>/<model>"]` in `config.toml`, then the provider's
model listing (cached for `providers.<id>.catalog_ttl_secs`, a day by
default), then the catalog built into Prometheus, then defaults (tools,
no images, no price). The context window has a longer chain: the
`context_window` pin of `[models."<provider>/<model>"]` (shown as
`200000 (pinned)`), the provider's listing, a local server probe, a window
discovered earlier for the same provider, model and endpoint (kept in the
profile's `state.db` and reused while the provider is unreachable), the
built-in catalog, a models.dev snapshot (`prometheus model list --refresh`
downloads a fresh one), a family default by model-name prefix (`claude-`,
`gpt-5`, `qwen3`...) and finally 128000 tokens. `prometheus model explain`
names the winning source. A session refuses to start on a model whose
window is below `model.min_context_window` (64000 tokens by default):

```console
$ prometheus config set model.min_context_window 200000
$ prometheus model explain --provider ollama --model qwen3  # exits 78
<>model 'ollama/qwen3' has a 32768-token context window, below the minimum of 200000 tokens<>
```

If the model's real window is larger than Prometheus knows, say so:

```toml
[models."ollama/qwen3"]
context_window = 262144
```

```console
$ prometheus config unset model.min_context_window
```

## Local servers

For `ollama`, `lmstudio`, `llamacpp`, `vllm` and `sglang`, Prometheus asks
the server itself. A `base_url` without `/v1` (`http://127.0.0.1:1234`) is
tried as `<root>/v1` and as given when you run `prometheus model set`; the
one that serves the model listing is saved and used for every request.
Models are listed through the server's own API when it has one (LM Studio
`/api/v1/models`, Ollama `/api/tags`), and the window the model actually
runs with is read from Ollama `/api/show` (its `num_ctx`), llama.cpp
`/props` (`n_ctx`), LM Studio's loaded context, or vLLM/SGLang
`max_model_len`. A session warns once when an Ollama model runs with
`num_ctx` of 2048 or less (raise it with `PARAMETER num_ctx 65536` in a
Modelfile or `OLLAMA_CONTEXT_LENGTH`), and when the local model reports no
tool-calling support, before the minimum-window check applies.

## Choosing a model

`prometheus model` is an interactive picker: choose a provider (those you
can already use are marked `connected`), then one of its models or type a
model id. It needs a terminal; in a script use `prometheus model set`:

```console
$ prometheus model  # exits 2
<>prometheus model requires a terminal; use 'prometheus model set <provider>/<model>'
```

`prometheus model set <provider>/<model> [--wire-format F] [--base-url
URL]` checks the route first (a usable credential, tool support and the
minimum context window) and only then writes `model.provider` and
`model.default` (and the provider's `wire_format` / `base_url` when given).
A route that cannot be used leaves `config.toml` untouched:

```console
$ prometheus model set deepseek/deepseek-chat  # exits 78
<>no credential for provider 'deepseek'<>
<>prometheus auth add deepseek<>
$ prometheus model set openrouter/anthropic/claude-sonnet-4.5
model set: openrouter/anthropic/claude-sonnet-4.5 (context window 1000000; wrote model.provider, model.default)
```

`prometheus model info` shows the route a new session would use, its
context window and where it came from, and the reasoning effort that
would be sent; its first line is the status-line segment:

```console
$ prometheus model info
openrouter/anthropic/claude-sonnet-4.5 · ctx 1000000 · reasoning medium · api_key:openrouter
```

`prometheus model list [--provider ID] [--refresh] [--json]` lists models
with their window and capabilities (`--refresh` fetches live listings and
the models.dev catalog first). `prometheus model list --providers [--json]`
lists every provider of the profile with its aliases, wire format,
endpoint and secret names, in resolution order (a plan-specific key such
as `ALIBABA_CODING_PLAN_API_KEY` is tried before the shared
`DASHSCOPE_API_KEY`). It never shows a secret value:

```console
$ prometheus model list --providers
openrouter (or)
alibaba-coding-plan (alibaba-coding, dashscope-coding)
<>chat_completions at https://coding-intl.dashscope.aliyuncs.com/v1; ALIBABA_CODING_PLAN_API_KEY, DASHSCOPE_API_KEY
```

Providers whose own wire format is implemented by a provider plugin
(`gemini` speaks `gemini_native`, `bedrock` `bedrock_converse`, `copilot`
comes from the `github-copilot` plugin) fail with
`provider.plugin_unavailable` until that plugin is installed; `auto`
passes over them. Gemini also answers OpenAI-style requests: set
`providers.gemini.base_url` to its `.../v1beta/openai` endpoint.

### Switching in a session

`/model [<provider>/]<model> [--provider ID] [--reasoning LEVEL]
[--once|--session|--global]` switches the model of a running session from
its next turn: `--once` for one turn, `--session` (the default once a model
is configured) until the session ends, `--global` also saves it as the
profile default (the default on a profile without a model). The target is
checked first; if it cannot be used the session keeps its route. Moving to
another model restarts the provider's prompt cache, which the notice says.

### Aliases

`[model.aliases]` names routes you use often. An alias is matched without
regard to case, wins over any other reading of the name, and works
wherever a model is named (`/model`, `prometheus model set`,
`prometheus fallback add`, auxiliary fallback chains). It brings its own
provider, endpoint and secret, so switching to it never reuses the
previous provider's key:

```toml
[model.aliases]
quick = "deepseek/deepseek-chat"

[model.aliases.fav]
provider = "openrouter"
model = "anthropic/claude-sonnet-4.5"
key_secret = "OPENROUTER_PERSONAL_KEY"
```

### A one-off endpoint

`--base-url URL` with `--api-key-stdin` uses an endpoint for one
invocation only (without `--provider` it is the `custom` provider). The
key is read from stdin, kept in memory, sent only to that URL's host and
never written to the secrets store, `config.toml`, trajectories or logs:

```console
$ echo -n "$TOKEN" | prometheus model explain --base-url https://llm.example.com/v1 --api-key-stdin --model my-model
credential ok flag
```

## Editing the fallback chain

`prometheus fallback list`, `add [<provider>/<model>]` (the picker when no
model is given), `remove <position|provider/model>` and `clear` edit
`[[fallback.chain]]`; new sessions use the result. `add` checks the route
like `model set` and refuses an entry the chain already has (the same
provider, under any alias, model and endpoint):

```console
$ prometheus fallback add openrouter/openai/gpt-5
added openrouter/openai/gpt-5 as fallback entry 1; new sessions use it
$ prometheus fallback add or/openai/gpt-5  # exits 75
<>openrouter/openai/gpt-5 is already in the fallback chain (entry 1); the chain is unchanged
$ prometheus fallback list
1. openrouter/openai/gpt-5
$ prometheus fallback clear
cleared the fallback chain (1 entries)
```

## Auxiliary models

Side tasks (`compression`, `title`, `vision`, `approval`, `memory`,
`curator`, `web_extract`, `skill_review`, `evolution`, `goal_judge`,
`side_question`, `voice_chat`) run on the session's own route unless
`[auxiliary.<task>]` pins one. When the main route is unavailable they
try the task's `fallback_chain` and then the profile's fallback chain;
they never pick some other provider just because a key for it is stored.
With nothing usable the task is skipped with a warning. Each attempt is
limited by `timeout_secs` (120 s for `vision` and `compression`, 30 s for
`approval` and `title`, 600 s for `curator` and `evolution`), retried on
the same route `auxiliary.transient_retries` times (default 2), and
recorded in the usage ledger as `auxiliary.<task>`:

```toml
[auxiliary.title]
provider = "openrouter"
model = "google/gemini-2.5-flash"
fallback_chain = ["quick"]   # an alias or <provider>/<model>; [] disables
```

## Reasoning effort

`agent.reasoning_effort` (`none`, `minimal`, `low`, `medium` (default),
`high`, `xhigh`, `max`, `ultra`) sets how hard reasoning models think.
`[agent.reasoning_overrides]` sets it per model; keys match model ids
without regard to case, `-`, `_`, `.` or a `provider/` prefix, so
`"gpt-5.5"` also matches `openrouter/openai/gpt_5_5`. A value can be a
level or a name from `[agent.reasoning_tiers]`. The level sent is lowered
to the most the model supports (never raised), and models that do not
reason get no reasoning setting at all. In a session,
`/reasoning <level> [--global]` changes it from the next request,
`/reasoning show|hide` toggles showing the reasoning and
`/reasoning full|clamp` shows it in full or clamped.

```toml
[agent.reasoning_tiers]
ultra = "max"

[agent.reasoning_overrides]
"gpt-5.5" = "high"
"claude-opus-4-1" = "ultra"
```
