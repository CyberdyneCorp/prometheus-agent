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
(`secret:<NAME>`, `pool:<provider>#<n>`, `oauth:<provider>`, `none`),
never by value:

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

A model's context window, output cap, capabilities and prices come from
`[models."<provider>/<model>"]` in `config.toml`, then the provider's
model listing (cached for `providers.<id>.catalog_ttl_secs`, a day by
default), then the catalog built into Prometheus, then defaults (128000
tokens, tools, no images, no price). A session refuses to start on a model
whose window is below `model.min_context_window` (64000 tokens by
default):

```console
$ prometheus config set model.min_context_window 200000
$ prometheus model explain --provider ollama --model qwen3  # exits 78
<>model 'ollama/qwen3' has a 128000-token context window, below the minimum of 200000 tokens<>
```

If the model's real window is larger than Prometheus knows, say so:

```toml
[models."ollama/qwen3"]
context_window = 262144
```
