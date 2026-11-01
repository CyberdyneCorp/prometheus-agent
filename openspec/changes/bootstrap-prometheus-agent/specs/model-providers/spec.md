# Model Providers

## Purpose
Model providers resolve which provider, model, endpoint, credential and wire format every Prometheus surface (CLI, gateway, cron, ACP, TUI, desktop, delegated children, auxiliary side calls) uses for a model call, and recover from provider failure through credential pools, health-aware fallback chains and routing policies. The key decision is that providers are typed data in one profile-scoped registry, resolved by a single deterministic and explainable resolution procedure driven only by `config.toml` and the secrets store, so no surface duplicates provider logic and no ambient environment variable can silently change routing.

## ADDED Requirements

### Requirement: Typed profile-scoped provider registry
The system SHALL maintain one provider registry per `ProfileScope`, populated from built-in provider descriptors, `[providers.<id>]` custom blocks in the profile's `config.toml`, and enabled provider plugins of that profile, and SHALL treat any provider id that is not a canonical id or declared alias in that registry as an error rather than a fallback.
**Milestone:** M1

#### Scenario: Plugin provider is invisible to another profile
- **GIVEN** one gateway process serving profiles `work` and `home`, with a provider plugin `acme` enabled only in `work`
- **WHEN** a session in `home` requests provider `acme`
- **THEN** the system SHALL fail with error code `provider.unknown` and SHALL NOT resolve the `work` profile's plugin

#### Scenario: Alias resolves to canonical id
- **GIVEN** a built-in provider `openrouter` declaring alias `or`
- **WHEN** a session requests provider `or`
- **THEN** the system SHALL resolve the provider with canonical id `openrouter` and record `openrouter` in the session metadata

### Requirement: Provider descriptor contract
The system SHALL describe every provider with a descriptor carrying `id`, `aliases` (default empty), `wire_format`, `base_url`, `auth` (`api_key` default, `oauth`, `token_mint`, `external_process`, `aws_chain`, `none`), `login_flows` (subset of `device` and `browser`, required when `auth = "oauth"`), `plan_notes` (default empty), `secret_names`, `key_hosts`, `models_url` (default `<base_url>/models`), `supports_model_listing` (default true), `supports_prompt_cache` (default false) and `hidden` (default false), and SHALL reject a descriptor with a missing required field, an unknown enum value, or a non-HTTPS `base_url` whose host is not loopback or a private-network address unless `allow_insecure_http = true` is set on that provider.
**Milestone:** M1

#### Scenario: Plain HTTP to a public host refused
- **GIVEN** `[providers.myproxy]` with `base_url = "http://llm.example.com/v1"` and no `allow_insecure_http`
- **WHEN** the profile config is loaded
- **THEN** the system SHALL fail validation with an error naming `providers.myproxy.base_url` and the `allow_insecure_http` remedy

#### Scenario: Local HTTP endpoint accepted
- **GIVEN** `[providers.ollama]` with `base_url = "http://127.0.0.1:11434/v1"`
- **WHEN** the profile config is loaded
- **THEN** the system SHALL accept the descriptor without `allow_insecure_http`

### Requirement: Provider registry changes apply at session boundaries
The system SHALL snapshot the provider registry when a session starts and SHALL use that snapshot for the life of the session, so installing, removing, enabling or editing a provider plugin or `[providers.*]` block takes effect for new sessions only, and SHALL apply it to a running session only through an explicit `/provider reload --now`, which emits a `provider.registry_reloaded` event.
**Milestone:** M2

#### Scenario: Plugin edited while a session runs
- **GIVEN** a running session using provider plugin `acme` version 1.0.0
- **WHEN** `acme` is upgraded to 1.1.0 and the session takes another turn
- **THEN** the system SHALL keep using the 1.0.0 descriptor for that session and SHALL use 1.1.0 for the next new session

### Requirement: Wire formats, selection and normalised event stream
The system SHALL support the wire formats `chat_completions`, `responses` and `anthropic_messages` natively and any additional wire format only through a provider plugin, and SHALL normalise every streaming response into one ordered internal event stream of `text_delta`, `reasoning_delta`, `tool_call_start`, `tool_call_delta`, `tool_call_end`, `usage` and exactly one terminal `stop` event carrying a `stop_reason` of `end_turn`, `tool_use`, `max_tokens`, `content_filter` or `error`. It SHALL select a session's wire format as the first of: an explicit `wire_format` in the provider's config block; a host-mandated format from the provider descriptor's host rules (for example a base URL path ending `/anthropic` or `/anthropic/v1` selects `anthropic_messages`); the descriptor's declared `wire_format`; otherwise `chat_completions`, and SHALL keep a declared `anthropic_messages` format only when the configured base URL host equals the descriptor's own host.
**Milestone:** M1

#### Scenario: Same tool call across wire formats
- **GIVEN** a recorded Anthropic Messages stream and a recorded Chat Completions stream that each request tool `read_file` with identical arguments
- **WHEN** both are normalised
- **THEN** the system SHALL produce identical `tool_call_start`/`tool_call_end` events with the same tool name and arguments and a `stop` event with `stop_reason = "tool_use"`

#### Scenario: Anthropic-declared provider behind a foreign relay
- **GIVEN** a provider whose descriptor declares `anthropic_messages` and a config `base_url` on an unrelated OpenAI-compatible relay host
- **WHEN** the wire format is selected
- **THEN** the system SHALL select `chat_completions`

#### Scenario: Unknown wire format value
- **GIVEN** `[providers.x] wire_format = "chat"`
- **WHEN** the config is validated
- **THEN** the system SHALL fail validation listing the accepted values

### Requirement: Requested provider precedence without ambient environment
The system SHALL determine the requested provider and model, in order, from: an explicit command-line flag (`--provider`, `--model`) or API argument; a session-scoped override set by `/model --session` or `/model --once`; the profile's `model.provider` and `model.default`; otherwise `auto`; and SHALL NOT read any non-secret environment variable to influence provider or model selection.
**Milestone:** M1

#### Scenario: Stale shell export is ignored
- **GIVEN** `model.provider = "anthropic"` in `config.toml` and an exported `PMT_INFERENCE_PROVIDER=openrouter`
- **WHEN** a new session resolves its provider
- **THEN** the system SHALL resolve `anthropic`

### Requirement: Deterministic explainable resolution (upgrade)
The system SHALL resolve a runtime route (provider id, model id, wire format, base URL, credential reference, source) through one ordered procedure — disabled-provider guard, provider lookup, endpoint resolution, credential resolution (explicit flag, credential pool, OAuth token, host-bound secret), capability check — in which each step either yields or fails with a typed error, and SHALL expose the full step-by-step trace through `prometheus model explain [--provider ID] [--model ID] [--json]` without performing a model call or printing any secret value.
**Milestone:** M1

#### Scenario: Explain shows the winning credential source
- **GIVEN** provider `openrouter` with a credential pool of two keys and an `OPENROUTER_API_KEY` secret
- **WHEN** the user runs `prometheus model explain --json`
- **THEN** the system SHALL print each resolution step with its outcome, SHALL identify the credential as `pool:openrouter#<n>`, and SHALL print no key material

### Requirement: Auto provider detection
The system SHALL resolve requested provider `auto` by trying, in order: a configured `model.provider`; providers that have a secret present in the secrets store in the order of `model.auto_order` (default `["anthropic", "openai", "openrouter", "google", "nous"]`); a logged-in OAuth provider; a reachable local endpoint configured under `[providers.*]`; and SHALL otherwise fail with code `provider.not_configured` and the message `Prometheus is not connected to any model provider. Run 'prometheus model' or 'prometheus auth add <provider>'.`, making a non-interactive command exit with status 78.
**Milestone:** M1

#### Scenario: Nothing configured in a one-shot run
- **GIVEN** a fresh profile with no provider config, no secrets and no OAuth login
- **WHEN** the user runs `prometheus chat -z "hello"`
- **THEN** the system SHALL print the not-connected message to stderr and exit with status 78

### Requirement: Unknown and disabled providers fail with typed errors
The system SHALL fail with code `provider.unknown` and a message beginning `Unknown provider '<name>'.` that includes the closest registered id by edit distance when one is within distance 2, and SHALL fail with code `provider.disabled` and the message `provider '<id>' is disabled in config (providers.<id>.enabled = false)` before any other resolution step when the requested or fallback provider has `providers.<id>.enabled = false`.
**Milestone:** M1

#### Scenario: Typo in --provider
- **GIVEN** `prometheus chat --provider opnrouter`
- **WHEN** the provider resolves
- **THEN** the system SHALL fail with `provider.unknown` suggesting `openrouter` and exit with status 78

#### Scenario: Disabled built-in provider
- **GIVEN** `[providers.openrouter] enabled = false`
- **WHEN** a session requests `openrouter`
- **THEN** the system SHALL fail with `provider.disabled` and SHALL NOT attempt credential resolution

### Requirement: Local endpoints never fall back to remote providers
The system SHALL fail with code `provider.missing_base_url` when a local-server provider (for example `ollama`, `vllm`, `sglang`, `lmstudio`) has no configured `base_url` and its descriptor's default loopback endpoint (`ollama` `http://127.0.0.1:11434/v1`, `lmstudio` `http://127.0.0.1:1234/v1`), if any, is unreachable (for `llamacpp` the `local-models` resolution applies instead and fails with `provider.local_runtime_down`), and SHALL fail with code `provider.missing_credential` when a provider whose `auth` is not `none` has no resolvable credential, and in neither case SHALL route the request to a different provider unless that provider is an explicit entry of the fallback chain.
**Milestone:** M1

#### Scenario: vLLM without an endpoint
- **GIVEN** `--provider vllm`, no `providers.vllm.base_url`, and an `OPENROUTER_API_KEY` secret
- **WHEN** the route resolves
- **THEN** the system SHALL fail with `provider.missing_base_url` and SHALL send no request to OpenRouter

#### Scenario: Ollama default endpoint unreachable
- **GIVEN** `--provider ollama`, no `providers.ollama.base_url`, nothing listening on `127.0.0.1:11434`, and an `OPENROUTER_API_KEY` secret
- **WHEN** the route resolves
- **THEN** the system SHALL fail with `provider.missing_base_url` naming the probed default endpoint and SHALL send no request to OpenRouter

### Requirement: Credentials are bound to their authoritative hosts
The system SHALL attach a secret to a request only when the request's exact hostname (case-insensitive, no substring or suffix match except an explicit leading `*.` wildcard) appears in that secret's `key_hosts` from the provider descriptor or from `providers.<id>.key_hosts`, and SHALL send a keyless request to a provider whose `auth = "none"`.
**Milestone:** M1

#### Scenario: Lookalike host does not receive a vendor key
- **GIVEN** a `DEEPSEEK_API_KEY` secret bound to `api.deepseek.com` and a custom provider with `base_url = "https://api.deepseek.com.attacker.test/v1"`
- **WHEN** the custom provider's request is built
- **THEN** the system SHALL NOT attach `DEEPSEEK_API_KEY`

#### Scenario: OpenAI key never reaches an aggregator
- **GIVEN** only an `OPENAI_API_KEY` secret bound to `api.openai.com`
- **WHEN** a request is built for `openrouter.ai`
- **THEN** the system SHALL NOT attach that key and SHALL fail with `provider.missing_credential`

### Requirement: Credential management commands
The system SHALL provide `prometheus auth add <provider> [--type api_key|oauth] [--label L] [--browser|--device]`, `prometheus auth list [--json]`, `prometheus auth status <provider> [--json]` and `prometheus auth remove <provider> <label|index>`, where `add` reads an API key from a hidden prompt or stdin (never from argv) for `api_key` providers and runs a sign-in flow for subscription providers, `list` and `status` show provider, label, credential kind (`api_key` or `subscription`), source, status (`ok`, `benched`, `dead`) and cooldown but never token or key material, and `remove` deletes the credential from the secrets store and revokes the refresh token at the provider when the provider exposes a revocation endpoint.
**Milestone:** M1

#### Scenario: Key supplied on argv refused
- **GIVEN** the user runs `prometheus auth add openai sk-abc123`
- **WHEN** the command parses its arguments
- **THEN** the system SHALL exit with status 2 and a usage error stating that keys are read from the prompt or stdin

#### Scenario: Listing never prints secrets
- **GIVEN** one stored OpenAI API key and one ChatGPT subscription sign-in
- **WHEN** the user runs `prometheus auth list --json`
- **THEN** the output SHALL show one `api_key` and one `subscription` credential with labels, sources and statuses and SHALL contain no substring of any key or token longer than its last 4 characters

### Requirement: Subscription and OAuth sign-in registry
The system SHALL support OAuth and subscription sign-in for every provider whose descriptor declares `auth = "oauth"` or `auth = "token_mint"`, shipping at least `anthropic` (Claude Max), `openai-codex` (ChatGPT plan), `xai-oauth` (SuperGrok / X Premium+), `nous` (Nous Portal), `qwen-oauth` (Qwen Portal), `minimax-oauth` (MiniMax portal), `openrouter` (OAuth key minting) and `vertex` (Google Cloud), SHALL let one provider hold API-key and subscription credentials side by side, choosing between them by `providers.<id>.prefer_credential` (`subscription` default, or `api_key`), and SHALL show the descriptor's `plan_notes` (eligibility and billing caveats, data in the provider descriptor or `[providers.<id>] plan_notes`, never text hardcoded in sign-in code) before every sign-in starts and in `prometheus auth status <provider>`; a provider's sign-in endpoints (client id, authorize, device, token and revocation URLs, scopes, redirect port and path, allowed hosts) come from data shipped with the provider and may be replaced field by field in `[providers.<id>.oauth]`, which also defines the sign-in of a custom `auth = "oauth"` provider.
**Milestone:** M1

#### Scenario: Subscription preferred over an API key
- **GIVEN** the `anthropic` provider with both an `ANTHROPIC_API_KEY` secret and a valid Claude Max sign-in, and default config
- **WHEN** a request is routed to `anthropic`
- **THEN** the system SHALL authenticate with the subscription credential and label the call `subscription:anthropic/<label>`

#### Scenario: Plan caveats shown before sign-in
- **GIVEN** the built-in `anthropic` descriptor whose `plan_notes` state that Claude Pro is not eligible and that Claude Max usage bills extra-usage credits
- **WHEN** the user runs `prometheus auth add anthropic --type oauth`
- **THEN** the system SHALL print those notes before opening the sign-in URL

#### Scenario: Caveat text comes from data
- **GIVEN** `[providers.anthropic] plan_notes = ["Custom note"]` in `config.toml`
- **WHEN** the user runs `prometheus auth status anthropic`
- **THEN** the output SHALL show `Custom note` in place of the built-in notes

### Requirement: Device-code and browser PKCE sign-in flows
The system SHALL implement two reusable sign-in flows for all OAuth providers: a device-code flow (printing the verification URL and user code, polling at the server-specified interval, tolerating up to 6 consecutive transport errors while polling, retrying the device-code request and token exchange up to 2 times, and failing with code `auth.login_expired` when the device code expires), and an authorization-code flow with PKCE (S256), a random `state` compared in constant time, and a loopback listener bound to the literal `127.0.0.1` on the provider's registered redirect port (or an OS-assigned port when the provider allows it); it SHALL reject a callback whose `state` or path nonce does not match, SHALL accept only HTTPS authorize and token endpoints (plain HTTP only for a loopback IdP), SHALL check the token endpoint host against the descriptor's allowed hosts before sending any request, SHALL print an `ssh -N -L <port>:127.0.0.1:<port>` tunnel hint when it detects a remote session without a local browser, and SHALL never log a token, code, verifier or `state`.
**Milestone:** M1

#### Scenario: Forged callback rejected
- **GIVEN** a browser sign-in waiting on its loopback listener
- **WHEN** a request arrives at the callback path with a `state` value different from the one generated
- **THEN** the system SHALL answer it with HTTP 400, SHALL NOT exchange its code, and SHALL keep waiting for the genuine callback

#### Scenario: Remote machine guidance
- **GIVEN** a session over SSH with no local browser
- **WHEN** a browser sign-in starts on port 1455
- **THEN** the system SHALL print the `ssh -N -L 1455:127.0.0.1:1455` tunnel hint alongside the authorization URL

#### Scenario: Transient polling errors tolerated
- **GIVEN** a device-code sign-in whose poll requests fail with 3 consecutive connection resets before the user approves
- **WHEN** the next poll succeeds
- **THEN** the system SHALL complete the sign-in without error

### Requirement: Anthropic Claude Max sign-in
The system SHALL sign in to `anthropic` with a Claude subscription through `prometheus auth add anthropic --type oauth` (browser PKCE), routing requests with the subscription token over `anthropic_messages` with the request identity the subscription requires, and SHALL report a plan that is not eligible for third-party use (for example Claude Pro) or an account without extra-usage credits as code `auth.plan_not_eligible` with the message suggesting `prometheus auth add anthropic` with an API key, never as a stale token.
**Milestone:** M1

#### Scenario: Ineligible plan reported clearly
- **GIVEN** a completed sign-in for a Claude Pro account
- **WHEN** the first inference request is rejected by the provider for plan eligibility
- **THEN** the system SHALL fail with `auth.plan_not_eligible`, SHALL keep the credential status `ok` but flag it `ineligible`, and SHALL NOT mark it `dead`

### Requirement: Borrowed CLI logins
The system SHALL, when `auth.adopt_external_logins = true` (default), use an existing sign-in owned by another installed agent CLI (the Claude Code credential store for `anthropic`, the Codex CLI `auth.json` for `openai-codex`) only when the profile holds no usable owned credential of that kind for the provider, reading it in place without copying it into the secrets store, labelling it `borrowed:<cli>` in `prometheus auth list`, announcing its first use in each session, and refreshing it only when the request that used it failed with an expired-token error, writing the rotated token back to the owning CLI's store so that CLI keeps working; with `auth.adopt_external_logins = false` the system SHALL never read or refresh another CLI's credentials.
**Milestone:** M1

#### Scenario: Owned grant beats borrowed login
- **GIVEN** a Prometheus-owned Claude Max sign-in and a Claude Code login on the same machine
- **WHEN** a request is routed to `anthropic`
- **THEN** the system SHALL use the owned sign-in and SHALL NOT read the Claude Code credential store

#### Scenario: Borrowing disabled
- **GIVEN** `auth.adopt_external_logins = false`, no owned `anthropic` credential and a valid Claude Code login
- **WHEN** a request is routed to `anthropic`
- **THEN** the system SHALL fail with `provider.missing_credential` and SHALL NOT read Claude Code's files

#### Scenario: Unrelated borrowed login is not rotated
- **GIVEN** an owned `anthropic` credential whose request failed with HTTP 401, and a borrowed Claude Code login present
- **WHEN** recovery runs
- **THEN** the system SHALL refresh only the owned credential and SHALL NOT refresh the borrowed login

### Requirement: OpenAI Codex ChatGPT plan sign-in
The system SHALL sign in to `openai-codex` with a ChatGPT plan through `prometheus auth add openai-codex`, using the device-code flow by default and the browser PKCE flow on the fixed redirect `http://localhost:1455/auth/callback` when `--browser` is passed or `providers.openai-codex.login_flow = "browser"`, falling back to the device-code flow with a message when port 1455 is in use, routing requests over the `responses` wire format with the account id from the token, and SHALL provide `prometheus auth import codex-cli [--path P]` that copies a Codex CLI sign-in (default `~/.codex/auth.json`, read-only) into the secrets store only when it holds both an access token and a refresh token, warning that both tools now share one rotating refresh-token family.
**Milestone:** M1

#### Scenario: Browser flow port busy
- **GIVEN** another process listening on `127.0.0.1:1455`
- **WHEN** the user runs `prometheus auth add openai-codex --browser`
- **THEN** the system SHALL print that port 1455 is in use and continue with the device-code flow

#### Scenario: Incomplete external file refused
- **GIVEN** `~/.codex/auth.json` containing an access token but no refresh token
- **WHEN** the user runs `prometheus auth import codex-cli`
- **THEN** the system SHALL import nothing, exit with status 1, and suggest `prometheus auth add openai-codex`

#### Scenario: Picker offers ChatGPT sign-in
- **GIVEN** a fresh profile
- **WHEN** the user runs `prometheus model` and chooses "ChatGPT or Codex subscription"
- **THEN** the system SHALL start the `openai-codex` sign-in and on success set `model.provider = "openai-codex"`

### Requirement: xAI SuperGrok sign-in
The system SHALL sign in to `xai-oauth` with a SuperGrok or X Premium+ subscription through browser PKCE, SHALL reuse the same subscription token for every xAI-backed tool (speech synthesis, image generation, video generation, transcription and X search) whose provider setting is `xai`, preferring it over an `XAI_API_KEY` unless `providers.xai-oauth.prefer_credential = "api_key"`, and SHALL report an HTTP 403 after a successful sign-in as code `auth.plan_not_eligible` stating that the subscription tier lacks API access and suggesting an `XAI_API_KEY`, never as a stale token.
**Milestone:** M2

#### Scenario: Tier restriction after sign-in
- **GIVEN** a successful `xai-oauth` sign-in whose inference request returns HTTP 403 for tier entitlement
- **WHEN** the error is handled
- **THEN** the system SHALL fail with `auth.plan_not_eligible` suggesting `prometheus auth add xai` and SHALL NOT mark the credential `dead`

#### Scenario: Tools share the subscription
- **GIVEN** a valid `xai-oauth` sign-in and no `XAI_API_KEY`
- **WHEN** the X search tool runs
- **THEN** it SHALL authenticate with the subscription token and label the call `subscription:xai-oauth/<label>`

### Requirement: Nous Portal subscription sign-in
The system SHALL sign in to `nous` through the device-code flow, preferring scoped short-lived inference tokens minted from the sign-in over long-lived session keys, SHALL offer every model the portal lists under that one sign-in, and SHALL let tools whose provider setting is `nous` (web search, image generation, speech synthesis, browser automation) bill the same subscription, showing the per-tool routing in `prometheus auth status nous`.
**Milestone:** M2

#### Scenario: One sign-in serves models and tools
- **GIVEN** a valid `nous` sign-in and `web.search_backend = "nous"`
- **WHEN** a session runs a chat turn and a web search
- **THEN** both calls SHALL carry `billing_source = "subscription:nous/<label>"`

### Requirement: Qwen Portal and MiniMax portal sign-in
The system SHALL sign in to `qwen-oauth` and `minimax-oauth` through browser PKCE, persist their refresh tokens in the secrets store, refresh them under the common refresh rules, and keep them separate from the API-key providers `alibaba`, `minimax` and `minimax-cn`, so that signing in to a portal never changes which credential an API-key provider uses.
**Milestone:** M2

#### Scenario: Portal and API-key providers stay separate
- **GIVEN** a `qwen-oauth` sign-in and a `DASHSCOPE_API_KEY` secret
- **WHEN** one session uses `qwen-oauth` and another uses `alibaba`
- **THEN** the first SHALL bill `subscription:qwen-oauth/<label>` and the second `api_key:alibaba/<label>`

### Requirement: OpenRouter OAuth key minting
The system SHALL support `prometheus auth add openrouter --type oauth`, which runs browser PKCE against OpenRouter, carries a random CSRF nonce in the loopback callback path (because the provider echoes no `state`), answers HTTP 404 for any other path, exchanges the code for a user API key, and stores that key in the credential pool as an `api_key` credential with source `oauth`; because no refresh token exists, a later 401 on that key SHALL mark it `dead` with the `auth.reauth_required` message.
**Milestone:** M2

#### Scenario: Wrong callback path ignored
- **GIVEN** an OpenRouter sign-in listening on a loopback port with nonce path `/cb/<nonce>`
- **WHEN** a request arrives at `/cb/other`
- **THEN** the system SHALL answer HTTP 404 and SHALL NOT exchange any code

### Requirement: Minted short-lived credentials
The system SHALL support credentials that are minted rather than stored — Google Vertex AI access tokens minted from a service-account JSON file (path in the secrets store) or Application Default Credentials, and any provider's `providers.<id>.key_cmd` helper whose stdout is a bare token or JSON with `access_token` and optional `expires_in` — caching each minted token until 60 seconds before its expiry (or 3000 seconds when none is advertised), re-minting once on a mid-session HTTP 401 before failing, rejecting multi-line helper output, and labelling such calls `billing_source = "api_key:<provider>/minted"`.
**Milestone:** M2

#### Scenario: Re-mint on mid-session 401
- **GIVEN** a `vertex` session whose cached access token is revoked server-side
- **WHEN** the next request returns HTTP 401
- **THEN** the system SHALL mint a new token, retry the request once, and continue the turn

#### Scenario: Ambiguous helper output rejected
- **GIVEN** a `key_cmd` that prints two lines
- **WHEN** a token is minted
- **THEN** the system SHALL fail with `provider.missing_credential` stating that helper output must be one token

### Requirement: Plugin OAuth and external-process providers
The system SHALL let a provider plugin declare a generic OAuth PKCE sign-in (client id, authorize URL, token URL, scopes, allowed hosts, redirect port) executed by the host's common flow so the plugin never sees the verifier or `state`, and SHALL support `auth = "external_process"` providers that drive an installed agent CLI (for example Claude Code) over the Agent Client Protocol using that CLI's own sign-in, resolving the CLI executable through the platform executable lookup and failing with code `provider.external_cli_missing` naming the command when it is not found.
**Milestone:** M2

#### Scenario: Missing external CLI
- **GIVEN** a provider with `auth = "external_process"` and `command = "claude"` not installed
- **WHEN** a session starts on it
- **THEN** the system SHALL fail with `provider.external_cli_missing` naming `claude`

#### Scenario: Plugin cannot read the PKCE verifier
- **GIVEN** a WASM provider plugin declaring an OAuth PKCE sign-in
- **WHEN** the sign-in completes
- **THEN** the plugin SHALL receive only an opaque credential handle, never the verifier, `state`, or refresh token

### Requirement: Subscription tokens live only in the secrets store
The system SHALL store every access token, refresh token, expiry, account id and plan label of an owned OAuth credential in the profile's secrets store (file mode 0600 or the OS keychain) and never in `config.toml`, `state.db`, logs, trajectories or any other plain file, SHALL keep each profile's sign-ins independent (no profile reads another profile's tokens), and SHALL serialise refreshes of one credential across all processes of the profile with a lock that re-reads the stored token after acquiring it, so a single-use rotating refresh token is never presented twice.
**Milestone:** M1

#### Scenario: Concurrent refresh uses the token once
- **GIVEN** a gateway and a CLI process of the same profile both holding an expired `openai-codex` access token
- **WHEN** both issue a request at the same moment
- **THEN** the system SHALL send exactly one refresh request to the provider and both processes SHALL use the rotated token

#### Scenario: Tokens absent from the trajectory
- **GIVEN** a completed session that used a subscription credential
- **WHEN** its trajectory and logs are searched for the access token value
- **THEN** no occurrence SHALL be found

### Requirement: Token refresh and dead-token quarantine
The system SHALL refresh an OAuth access token at least 60 seconds before its expiry and on an HTTP 401 from the inference endpoint, refreshing only the credential used by the failed request, and when the token endpoint answers with a terminal failure (HTTP 400, 401 or 403, `invalid_grant`, `invalid_token`, or `refresh_token_reused`) SHALL mark the credential `dead`, SHALL never present that refresh token again, and SHALL fail later requests for it with code `auth.reauth_required` and the message `Sign-in for <provider> (<label>) expired or was revoked. Run 'prometheus auth add <provider>' to sign in again.` until a new sign-in for that label succeeds; a rate-limited token endpoint (HTTP 429) SHALL bench the credential instead of marking it dead.
**Milestone:** M1

#### Scenario: Revoked refresh token quarantined
- **GIVEN** an `openai-codex` credential whose refresh request returns `invalid_grant`
- **WHEN** three more requests are routed to it
- **THEN** the system SHALL send no further refresh requests and each request SHALL fail with `auth.reauth_required` (or activate a fallback entry when one is configured)

#### Scenario: Quarantine cleared by a new sign-in
- **GIVEN** a `dead` credential labelled `personal`
- **WHEN** `prometheus auth add openai-codex --label personal` completes successfully
- **THEN** the credential status SHALL be `ok` and requests SHALL use it again

### Requirement: Subscription plan and quota visibility (upgrade)
The system SHALL, for subscription providers that expose a usage endpoint or rate-limit headers, show the plan label and each quota window's used percentage and reset time in `prometheus auth status <provider>`, in `/usage`, and in a `subscription.quota` field of the session status shown by every surface, refreshing it at most once per 60 seconds per credential; when a quota window reaches 100% the credential SHALL be benched until that window's reset time, and a later probe showing the quota restored early SHALL clear the bench; providers without such data SHALL be shown as `quota: not reported by provider`.
**Milestone:** M2

#### Scenario: Codex quota shown
- **GIVEN** a ChatGPT subscription whose usage endpoint reports a 5-hour window at 72% resetting at 14:00 and a weekly window at 30%
- **WHEN** the user runs `prometheus auth status openai-codex`
- **THEN** the output SHALL list the plan label and both windows with their percentages and reset times

#### Scenario: Exhausted quota benched until reset
- **GIVEN** a subscription whose 5-hour window reports 100% with reset in 40 minutes
- **WHEN** a request is routed to that provider
- **THEN** the system SHALL skip the credential (or activate a fallback) and report the reset time, without sending the inference request

### Requirement: Billing source transparency (upgrade)
The system SHALL label every model call with a `billing_source` in the grammar of `observability` › Per-call usage ledger (`subscription:<provider>/<label>`, `api_key:<provider>/<label>` or `local` for model calls; `keyless:<vendor>` only for opted-in web tools), recording it on the `model.call` trajectory event, the trajectory and the usage ledger, SHALL show the current session's billing source in the CLI status line, TUI, desktop and dashboard, and SHALL state the billing change in the `provider.fallback` notice (for example `now billing API key openrouter/default instead of subscription openai-codex/personal`) whenever a route change moves between a subscription and a pay-per-token credential; when `providers.<id>.confirm_metered_fallback = true`, an interactive session SHALL ask before making such a move and a non-interactive one SHALL fail the turn instead.
**Milestone:** M2

#### Scenario: Fallback from subscription to API key is announced
- **GIVEN** a session on an `openai-codex` subscription with a fallback entry on `openrouter` using an API key
- **WHEN** the subscription's quota is exhausted mid-turn and fallback activates
- **THEN** the `provider.fallback` event SHALL carry `billing_change = true` and the notice SHALL name both billing sources

#### Scenario: Ledger distinguishes subscription calls
- **GIVEN** a session with 3 calls on a subscription and 2 on an API key
- **WHEN** the user runs `prometheus usage --session <id> --json`
- **THEN** the output SHALL group the 5 calls by `billing_source` with subscription calls carrying `cost_status = "subscription"`

### Requirement: Credential pool selection strategies
The system SHALL select among multiple credentials for one provider using `providers.<id>.pool_strategy` with values `fill_first` (default), `round_robin`, `random` and `least_used`, SHALL fail config validation for any other value, and SHALL increment the selected credential's `request_count` in `state.db` for every request it serves.
**Milestone:** M1

#### Scenario: Invalid strategy fails validation
- **GIVEN** `[providers.openrouter] pool_strategy = "weighted"`
- **WHEN** the config is validated
- **THEN** the system SHALL fail validation naming `providers.openrouter.pool_strategy` and listing the accepted values

#### Scenario: Round robin rotates
- **GIVEN** a pool of credentials A and B with `pool_strategy = "round_robin"`
- **WHEN** four requests are served
- **THEN** the credentials SHALL be used in the order A, B, A, B

### Requirement: Exhausted credential cooldowns
The system SHALL bench a failing pooled credential until the provider-supplied reset time when one is present and in the future, otherwise for 300 seconds after HTTP 401, 3600 seconds after HTTP 429, and 3600 seconds after HTTP 402 or a `billing` classification; when the credential is the provider's only non-revoked credential and the failure is not `billing`, the bench SHALL be at most 60 seconds; benches SHALL persist in `state.db` across restarts.
**Milestone:** M1

#### Scenario: Sole credential throttled recovers quickly
- **GIVEN** a provider with one credential exhausted by HTTP 429 without a reset header
- **WHEN** 60 seconds have elapsed
- **THEN** the credential SHALL be selectable again

#### Scenario: Billing failure keeps the full bench
- **GIVEN** a provider with one credential that failed with HTTP 402
- **WHEN** 60 seconds have elapsed
- **THEN** the credential SHALL remain benched until 3600 seconds have elapsed

### Requirement: Error classification and same-route retries
The system SHALL classify every failed provider call into exactly one of `auth`, `entitlement`, `billing`, `rate_limit`, `overloaded`, `server_error`, `timeout`, `network`, `context_overflow`, `content_filter`, `invalid_request` and `config`, and SHALL treat only `rate_limit`, `overloaded`, `server_error`, `timeout` and `network` as retryable on the same route and only `auth`, `entitlement`, `billing`, `rate_limit`, `overloaded`, `server_error`, `timeout` and `network` as eligible for fallback. It SHALL retry a retryable failure on the same route up to `providers.<id>.max_retries` times (default 2, range 0-6) with exponential backoff starting at 1 second, capped at 30 seconds, with full jitter, honouring a `Retry-After` header of at most 60 seconds, and SHALL apply `providers.<id>.request_timeout_secs` (default 600) per request and `providers.<id>.stream_idle_timeout_secs` (default 120) between stream events.
**Milestone:** M1

#### Scenario: Misconfiguration is never rerouted
- **GIVEN** a primary route that fails with `invalid_request` because a tool schema is malformed, and a configured fallback chain
- **WHEN** the error is classified
- **THEN** the system SHALL surface the error to the turn loop and SHALL NOT activate any fallback entry

#### Scenario: Plan entitlement is not a dead token
- **GIVEN** a successful subscription sign-in whose inference request returns HTTP 403 stating the plan tier lacks API access
- **WHEN** the error is classified
- **THEN** the system SHALL classify it as `entitlement`, SHALL NOT mark the credential `dead`, and SHALL tell the user the plan tier is not eligible

#### Scenario: Idle stream times out
- **GIVEN** a streaming response that emits no event for 121 seconds with default config
- **WHEN** the idle timer fires
- **THEN** the system SHALL abort the request and classify it as `timeout`

### Requirement: Fallback chain configuration
The system SHALL build a profile's fallback chain from the ordered `[[fallback.chain]]` array, each entry requiring `provider` and `model` and optionally `base_url` and `credential` (a secret name), SHALL de-duplicate entries by (canonical provider id, lower-cased model, normalised base URL without trailing slash) keeping the first, SHALL fail validation for an entry with an unknown provider id, and SHALL default to an empty chain.
**Milestone:** M1

#### Scenario: Alias and canonical duplicates collapse
- **GIVEN** two chain entries for the same model, one under alias `or` and one under `openrouter`
- **WHEN** the chain is built
- **THEN** the system SHALL keep only the first entry

### Requirement: In-turn fallback activation and turn-scoped restore
The system SHALL, when a call fails with a fallback-eligible class after same-route retries are exhausted, walk the fallback chain in order from the entry after the current one, skipping entries that are disabled, lack a credential, have every pooled credential benched for more than 600 seconds, have a health score below `fallback.min_health` (default 0.2), or resolve to the same (provider, model, base URL) that just failed, and on success SHALL switch the route for the remainder of the turn and emit a `provider.fallback` event carrying `from`, `to` and `reason` that every surface renders as a notice. It SHALL restore the primary route at the start of each new turn unless the primary is inside a cooldown, SHALL arm a primary cooldown when leaving it for `rate_limit` or `billing` equal to the provider's reset time when valid, otherwise `min(60 * 2^n, 14400)` seconds for the n-th consecutive failover, and SHALL reset the consecutive-failover counter after a successful primary turn.
**Milestone:** M1

#### Scenario: Candidate with an exhausted pool is skipped
- **GIVEN** a chain `[B, C]` where every credential of B is benched for 2 hours
- **WHEN** the primary fails with `overloaded`
- **THEN** the system SHALL skip B, activate C, and emit `provider.fallback` with `to.provider = "C"`

#### Scenario: Chain exhausted
- **GIVEN** a chain whose every entry is unusable
- **WHEN** the primary fails with `server_error`
- **THEN** the system SHALL surface the primary error to the turn loop and arm a primary cooldown of 5 seconds

#### Scenario: Backoff caps at four hours
- **GIVEN** repeated consecutive rate-limit failovers without a reset header
- **WHEN** the cooldown is armed each time
- **THEN** the cooldown SHALL double from 60 seconds and never exceed 14400 seconds

#### Scenario: Primary in cooldown stays on fallback
- **GIVEN** an active fallback and a primary cooldown ending in 10 minutes
- **WHEN** the next turn starts
- **THEN** the system SHALL keep the fallback route for that turn

### Requirement: Scoped fallback chains for pinned owners
The system SHALL give a delegated child, cron job or harness-evolution session that pins its own provider or model no inherited fallback chain unless it declares one, SHALL let an unpinned owner inherit the parent session's chain, and SHALL treat an explicit empty `fallback_chain = []` as fallback disabled.
**Milestone:** M2

#### Scenario: Pinned cron job
- **GIVEN** a cron job with its own `provider` and `model` and no `fallback_chain`
- **WHEN** its route fails with `overloaded`
- **THEN** the system SHALL NOT activate any fallback entry and SHALL report the failure on the job run

### Requirement: Prompt caching and stable prefix across route changes
The system SHALL carry cache breakpoints on the internal request type and translate them per wire format (explicit `cache_control` markers for `anthropic_messages`, a stable prompt cache key derived from the session id for `responses` and `chat_completions` providers that declare `supports_prompt_cache`), and SHALL report `cache_read_tokens` and `cache_write_tokens` in every `usage` event, using 0 when the provider does not report them. It SHALL keep the provider and model identity out of the persisted system prompt text and SHALL NOT modify the system prompt, tool schemas or prior messages when a fallback activates, a primary is restored, or a `/model` switch applies, so that returning to a previously used route replays a byte-identical prefix.
**Milestone:** M1

#### Scenario: Cache key stable across turns
- **GIVEN** a session on an OpenAI Responses provider that supports prompt caching
- **WHEN** two consecutive turns are sent
- **THEN** both requests SHALL carry the same prompt cache key

#### Scenario: Restored primary replays the original prefix
- **GIVEN** a session whose first turn ran on the primary, whose second turn ran on a fallback
- **WHEN** the third turn is sent to the restored primary
- **THEN** the system prompt and tool schema bytes sent SHALL be identical to those of the first turn

### Requirement: Model metadata catalog
The system SHALL resolve a model's metadata (`context_window`, `max_output_tokens`, `supports_tools`, `supports_vision`, `supports_video` (video input), `supports_reasoning`, per-million-token input/output/cache prices) in order from: `[models."<provider>/<model>"]` overrides in `config.toml`; the provider's live model listing cached for `providers.<id>.catalog_ttl_secs` (default 86400); the built-in catalog shipped with the binary; and otherwise defaults of a 128000-token context window, tools supported, vision unsupported and unknown prices, logging one WARN per (provider, model) per process naming the override key.
**Milestone:** M1

#### Scenario: Explicit override wins without network access
- **GIVEN** `[models."local/qwen3"] context_window = 131072`
- **WHEN** the metadata for `local/qwen3` is resolved
- **THEN** the system SHALL return 131072 without any network request

#### Scenario: Provider without a catalog
- **GIVEN** a provider with `supports_model_listing = false`
- **WHEN** the model picker is built
- **THEN** the system SHALL make no listing request and SHALL offer the built-in catalog entries for that provider

### Requirement: Capability-aware route validation and minimum context window
The system SHALL refuse to route a request to a model whose metadata lacks a capability the request needs — `supports_tools` when tool schemas are present, `supports_vision` when image parts are present, or a context window smaller than the request's estimated tokens — failing with code `model.capability_missing` naming the capability, and SHALL apply the same check when choosing fallback and routing-policy candidates so incapable candidates are skipped. It SHALL refuse to start a session whose resolved model context window is below `model.min_context_window` (default 64000 tokens, floor 16000) with code `model.context_too_small` and a message naming the model, its window, the minimum, and the `[models."<provider>/<model>"] context_window` override, exiting a non-interactive command with status 78.
**Milestone:** M1

#### Scenario: Vision request skips a text-only fallback
- **GIVEN** a turn with an image attachment, a failing primary and a fallback chain `[text-only, vision-capable]`
- **WHEN** fallback activates
- **THEN** the system SHALL skip the text-only entry and activate the vision-capable entry

#### Scenario: Small local window refused
- **GIVEN** a local endpoint whose model reports a 32768-token window and default config
- **WHEN** a session starts on it
- **THEN** the system SHALL fail with `model.context_too_small` and SHALL send no model request

### Requirement: Per-call cost accounting
The system SHALL compute the cost of every provider call from the resolved model prices and the reported usage (input, output, cache read, cache write and reasoning tokens), SHALL record it in the usage ledger keyed by the session's root lineage with `cost_status` of `exact` (provider-reported cost), `estimated` (computed from catalog prices), `subscription` (covered by a subscription plan; marginal cost recorded as 0 and token usage kept) or `unknown` (no prices), and SHALL never record an `unknown` cost as zero in totals.
**Milestone:** M1

#### Scenario: Unknown price
- **GIVEN** a call to a model with no price metadata
- **WHEN** the call completes
- **THEN** the ledger row SHALL have `cost_status = "unknown"` and `prometheus usage` SHALL report the session total as a lower bound marked with `+`

### Requirement: Provider health scoring (upgrade)
The system SHALL maintain a health record per (provider, model, credential) in `state.db` over a rolling window of the last `routing.health_window` calls (default 50), exposing success rate, error counts by class, p50 and p95 time-to-first-token and p50 output tokens per second, combining them into a score in [0.0, 1.0] that decays toward 1.0 with a half-life of `routing.health_half_life_secs` (default 1800) when no calls are made, and SHALL show it through `prometheus providers health [--json]`.
**Milestone:** M2

#### Scenario: Repeated overloads lower the score
- **GIVEN** a model whose last 10 calls all failed with `overloaded`
- **WHEN** `prometheus providers health --json` runs
- **THEN** that model's score SHALL be below 0.2 and its `errors.overloaded` count SHALL be 10

#### Scenario: Idle provider recovers
- **GIVEN** a model with score 0.1 and no calls for 3600 seconds under default config
- **WHEN** its score is read
- **THEN** the score SHALL be at least 0.7

### Requirement: Cost and latency aware routing policy (upgrade)
The system SHALL choose a session's route at session start using `routing.policy`, with values `pinned` (default: the requested provider and model), `cheapest` (lowest estimated cost per turn), `fastest` (lowest p50 time-to-first-token) and `balanced` (lowest `routing.cost_weight * normalised_cost + (1 - routing.cost_weight) * normalised_latency`, `cost_weight` default 0.5), over the candidate set `routing.candidates` (default: the requested route plus the fallback chain) filtered by credentials, health score at least `fallback.min_health` and capability checks, SHALL record the decision and its inputs in a `provider.route_selected` event, and SHALL NOT change the route mid-session except through fallback or `/model`.
**Milestone:** M3

#### Scenario: Cheapest healthy capable candidate wins
- **GIVEN** `routing.policy = "cheapest"` and candidates A (cheapest, health 0.1), B (second cheapest, health 0.9), C (most expensive, health 1.0)
- **WHEN** a session starts
- **THEN** the system SHALL select B and emit `provider.route_selected` naming A as rejected for health

#### Scenario: Route is stable within a session
- **GIVEN** a session that selected B under `routing.policy = "fastest"`
- **WHEN** C becomes faster than B during the session
- **THEN** subsequent turns in that session SHALL stay on B

### Requirement: Provider spend caps (upgrade)
The system SHALL enforce optional `providers.<id>.daily_budget_usd` and `providers.<id>.monthly_budget_usd` limits against the profile's usage ledger, treating a provider whose cap is reached as unavailable with error class `billing` and code `provider.budget_exhausted` so the fallback chain applies, and SHALL emit one WARN event per provider per period when 80% of a cap is consumed.
**Milestone:** M2

#### Scenario: Monthly cap reached falls back
- **GIVEN** `[providers.anthropic] monthly_budget_usd = 50.0`, $50.10 spent this month, and a fallback entry on `openrouter`
- **WHEN** a new request is routed to `anthropic`
- **THEN** the system SHALL send no request to `anthropic` and SHALL activate the `openrouter` fallback with reason `provider.budget_exhausted`

### Requirement: Auxiliary task routing
The system SHALL route side-model calls by a typed auxiliary task enum (`compression`, `title`, `vision`, `approval`, `memory`, `curator`, `web_extract`, `skill_review`, `evolution`, `goal_judge` (owned by `session-automation`), `side_question` (`agent-turn-loop`) and `voice_chat` (`voice-mode`), the last three defaulting to the session's main model), reading `[auxiliary.<task>]` keys `provider`, `model`, `timeout_secs` and `fallback_chain` with precedence explicit call argument over config over `auto`, SHALL fail config validation for an unknown task name, and SHALL default timeouts to 120 seconds for `vision` and `compression`, 30 seconds for `approval` and `title`, and 600 seconds for `curator` and `evolution`. It SHALL retry a retryable auxiliary failure on the same route `auxiliary.transient_retries` times (default 2, range 0-6), SHALL emit `aux_call.start` and `aux_call.end` events once per attempt and never the main-loop `model.call` event for auxiliary calls, and SHALL record auxiliary cost in the usage ledger under the owning session with `kind = "auxiliary"` and the task name.
**Milestone:** M1

#### Scenario: Per-task model pin
- **GIVEN** `[auxiliary.compression] provider = "openrouter"` and `model = "google/gemini-3.6-flash"`
- **WHEN** a compression call resolves without explicit arguments
- **THEN** the system SHALL route it to that provider and model

#### Scenario: Misspelled task section
- **GIVEN** `[auxiliary.compresion]` in `config.toml`
- **WHEN** the config is validated
- **THEN** the system SHALL fail validation naming the unknown task

#### Scenario: Aux call emits aux events only
- **GIVEN** an observer subscribed to both `model.call` and `aux_call.start`
- **WHEN** a compression auxiliary call runs
- **THEN** only `aux_call.start` SHALL be delivered for that call

### Requirement: Auxiliary auto route never guesses another account
The system SHALL resolve an `auto` auxiliary route to the session's current main route, then to the task's `fallback_chain`, then to the profile's fallback chain, and SHALL return no route (the auxiliary feature degrades with a WARN naming the task) rather than selecting any other credentialed provider when all of those are unavailable.
**Milestone:** M1

#### Scenario: Main provider down without a chain
- **GIVEN** a main provider that is unavailable, no fallback chains, and a stored key for an unrelated provider
- **WHEN** a `title` auxiliary call resolves with `auto`
- **THEN** the system SHALL NOT use the unrelated provider and SHALL skip title generation with a WARN

### Requirement: /model switch command
The system SHALL accept `/model [<provider>/]<model> [--provider ID] [--reasoning none|minimal|low|medium|high|xhigh|max] [--once|--session|--global]`, where `--once` applies to the next turn only, `--session` (the default when a model is already configured) applies until the session ends, and `--global` also writes `model.provider` and `model.default` to `config.toml`; it SHALL reject `--once` combined with `--global` with `/model --once cannot be combined with --global`, SHALL validate the target route (credentials and capabilities) before switching, and SHALL leave the current route unchanged when validation fails.
**Milestone:** M1

#### Scenario: Invalid target leaves the route unchanged
- **GIVEN** a session on `anthropic/claude-x` and no credential for `mistral`
- **WHEN** the user runs `/model mistral/large`
- **THEN** the system SHALL report `provider.missing_credential` and the next turn SHALL still use `anthropic/claude-x`

#### Scenario: First pick on a fresh profile persists
- **GIVEN** a profile with no `model.provider` or `model.default`
- **WHEN** the user runs `/model openrouter/some-model` without a scope flag
- **THEN** the system SHALL write the pick to `config.toml` as with `--global`

### Requirement: prometheus model command
The system SHALL provide `prometheus model` as an interactive picker when stdin and stdout are terminals, `prometheus model set <provider>/<model> [--wire-format F] [--base-url URL]` as its non-interactive equivalent, and `prometheus model list [--provider ID] [--refresh] [--json]`, and SHALL exit with status 2 and the message `prometheus model requires a terminal; use 'prometheus model set <provider>/<model>'` when the picker is invoked without a terminal.
**Milestone:** M1

#### Scenario: Piped invocation
- **GIVEN** `echo | prometheus model`
- **WHEN** the command starts
- **THEN** the system SHALL print the terminal-required message to stderr and exit with status 2

#### Scenario: Non-interactive set validates first
- **GIVEN** no credential for provider `groq`
- **WHEN** the user runs `prometheus model set groq/llama-x`
- **THEN** the system SHALL exit with status 78 naming `prometheus auth add groq` and SHALL NOT modify `config.toml`

### Requirement: Model retirement table
The built-in model catalog MUST carry a retirement table mapping each retired or deprecated `<provider>/<model>` id to its status (`deprecated` or `retired`), an optional retirement date and a replacement id, updated with each release and overridable per id by `[models."<provider>/<model>"] replacement`; resolving a `retired` id for a new session SHALL fail with exit 78 naming the replacement and `prometheus config fix-models` (`configuration-and-profiles` › Retired model rewrite), while a `deprecated` id SHALL still be used with one WARN per process, and `prometheus model list` SHALL mark both.
**Milestone:** M2

#### Scenario: Retired default model
- **GIVEN** `model.default = "xai/grok-2"` marked `retired` with replacement `xai/grok-4`
- **WHEN** the user starts a chat
- **THEN** the system SHALL exit 78 with a message naming `xai/grok-4` and `prometheus config fix-models`

### Requirement: Built-in provider catalog
The system SHALL ship the following built-in provider descriptors (canonical id; aliases; secret names in resolution order; default `base_url`; `wire_format`; `auth`), each also usable as a `[providers.<id>]` override target, and SHALL treat a descriptor's `secret_names` as an ordered list in which the first name present in the secrets store supplies the credential, so a plan-specific key wins over a shared vendor key:

| id | aliases | secret_names (ordered) | base_url | wire_format | auth |
|---|---|---|---|---|---|
| `openrouter` | or | OPENROUTER_API_KEY | https://openrouter.ai/api/v1 | chat_completions | api_key / oauth (key minting) |
| `anthropic` | claude, claude-oauth, claude-code | ANTHROPIC_API_KEY, ANTHROPIC_TOKEN | https://api.anthropic.com | anthropic_messages | api_key / oauth |
| `openai-api` | openai | OPENAI_API_KEY | https://api.openai.com/v1 | responses | api_key |
| `openai-codex` | codex, chatgpt, chatgpt-codex | (sign-in) | https://chatgpt.com/backend-api/codex | responses | oauth |
| `nous` | nous-portal, nousresearch | NOUS_API_KEY | https://inference-api.nousresearch.com/v1 | chat_completions | oauth (device) / api_key |
| `xai` | grok, x-ai, x.ai | XAI_API_KEY | https://api.x.ai/v1 | responses (`supports_prompt_cache = true`) | api_key |
| `xai-oauth` | grok-oauth, x-ai-oauth, xai-grok-oauth | (sign-in) | https://api.x.ai/v1 | responses | oauth |
| `router` (Ramp Router) | ramp-router, ramp, router.com | RAMP_ROUTER_API_KEY, ROUTER_API_KEY | https://api.router.com/v1 | responses | api_key |
| `fireworks` | fireworks-ai, fw | FIREWORKS_API_KEY | https://api.fireworks.ai/inference/v1 | chat_completions | api_key |
| `novita` | novita-ai, novitaai | NOVITA_API_KEY | https://api.novita.ai/openai/v1 | chat_completions | api_key |
| `ai-gateway` (Vercel) | vercel, vercel-ai-gateway, aigateway | AI_GATEWAY_API_KEY | https://ai-gateway.vercel.sh/v1 | chat_completions | api_key |
| `zai` | glm, z-ai, z.ai, zhipu | GLM_API_KEY, ZAI_API_KEY, Z_AI_API_KEY | https://api.z.ai/api/paas/v4 (auto-detected) | chat_completions | api_key |
| `kimi-coding` | kimi, moonshot, kimi-for-coding | KIMI_API_KEY, KIMI_CODING_API_KEY | https://api.moonshot.ai/v1 (auto-detected) | chat_completions | api_key |
| `kimi-coding-cn` | kimi-cn, moonshot-cn | KIMI_CN_API_KEY | https://api.moonshot.cn/v1 | chat_completions | api_key |
| `arcee` | arcee-ai, arceeai | ARCEEAI_API_KEY | https://api.arcee.ai/api/v1 | chat_completions | api_key |
| `gmi` | gmi-cloud, gmicloud | GMI_API_KEY | https://api.gmi-serving.com/v1 | chat_completions | api_key |
| `nebius-token-factory` | nebius, nebius-tf, token-factory, tokenfactory | NEBIUS_API_KEY, NEBIUS_TOKEN_FACTORY_API_KEY | https://api.tokenfactory.nebius.com/v1 | chat_completions | api_key |
| `actual` | actual-computer, actualcomputer, aci | ACTUAL_API_KEY | https://api.actual.inc/v1 | chat_completions | api_key (hosted) / none (loopback daemon) |
| `minimax` | mini-max | MINIMAX_API_KEY | https://api.minimax.io/anthropic | anthropic_messages | api_key |
| `minimax-cn` | minimax-china | MINIMAX_CN_API_KEY | https://api.minimaxi.com/anthropic | anthropic_messages | api_key |
| `minimax-oauth` | minimax-portal, minimax-global | (sign-in) | https://api.minimax.io/anthropic | anthropic_messages | oauth |
| `alibaba` | dashscope, alibaba-cloud, qwen-dashscope, aliyun | DASHSCOPE_API_KEY | https://dashscope-intl.aliyuncs.com/compatible-mode/v1 | chat_completions | api_key |
| `alibaba-cn` | dashscope-cn, alibaba-cloud-cn | DASHSCOPE_API_KEY | https://dashscope.aliyuncs.com/compatible-mode/v1 | chat_completions | api_key |
| `alibaba-coding-plan` | alibaba-coding, dashscope-coding | ALIBABA_CODING_PLAN_API_KEY, DASHSCOPE_API_KEY | https://coding-intl.dashscope.aliyuncs.com/v1 | chat_completions | api_key |
| `alibaba-coding-plan-cn` | alibaba-coding-cn, dashscope-coding-cn | ALIBABA_CODING_PLAN_CN_API_KEY, ALIBABA_CODING_PLAN_API_KEY, DASHSCOPE_API_KEY | https://coding.dashscope.aliyuncs.com/v1 | chat_completions | api_key |
| `alibaba-token-plan` | dashscope-token-plan | ALIBABA_TOKEN_PLAN_API_KEY | https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1 | chat_completions | api_key |
| `alibaba-token-plan-cn` | dashscope-token-plan-cn | ALIBABA_TOKEN_PLAN_CN_API_KEY, ALIBABA_TOKEN_PLAN_API_KEY | https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1 | chat_completions | api_key |
| `qwen-oauth` | qwen, qwen-portal, qwen-cli | (sign-in) | https://portal.qwen.ai/v1 | chat_completions | oauth |
| `kilocode` | kilo-code, kilo, kilo-gateway | KILOCODE_API_KEY | https://api.kilo.ai/api/gateway | chat_completions | api_key |
| `xiaomi` | mimo, xiaomi-mimo | XIAOMI_API_KEY | https://api.xiaomimimo.com/v1 | chat_completions | api_key |
| `tencent-tokenhub` | tokenhub | TOKENHUB_API_KEY | https://tokenhub.tencentmaas.com/v1 | chat_completions | api_key |
| `tencent-tokenplan` | tokenplan | TOKENPLAN_API_KEY | https://api.lkeap.cloud.tencent.com/plan/anthropic | anthropic_messages | api_key |
| `opencode-zen` | opencode, zen | OPENCODE_ZEN_API_KEY | https://opencode.ai/zen/v1 | per-model | api_key |
| `opencode-go` | go, opencode-go-sub | OPENCODE_GO_API_KEY | https://opencode.ai/zen/go/v1 | per-model | api_key |
| `commandcode` | commandcode-chat | COMMANDCODE_API_KEY | https://api.commandcode.ai/provider/v1 | chat_completions | api_key |
| `commandcode-anthropic` | commandcode-claude | COMMANDCODE_API_KEY | https://api.commandcode.ai/provider/v1 | anthropic_messages | api_key |
| `deepseek` | deepseek-chat, deep-seek | DEEPSEEK_API_KEY | https://api.deepseek.com/v1 | chat_completions | api_key |
| `huggingface` | hf, hugging-face | HF_TOKEN | https://router.huggingface.co/v1 | chat_completions | api_key |
| `gemini` | google, google-gemini, google-ai-studio | GOOGLE_API_KEY, GEMINI_API_KEY | https://generativelanguage.googleapis.com/v1beta | gemini_native (bundled plugin) | api_key |
| `vertex` | google-vertex, vertex-ai, gcp-vertex | (service account / ADC) | computed from project and region | chat_completions | token_mint |
| `bedrock` | aws, aws-bedrock, amazon-bedrock | (AWS credential chain) | https://bedrock-runtime.<region>.amazonaws.com | bedrock_converse (bundled plugin) | aws_chain |
| `azure-foundry` | azure, azure-ai-foundry, azure-ai | AZURE_FOUNDRY_API_KEY | per-resource (required) | auto-detected | api_key / Entra ID |
| `nvidia` | nvidia-nim, nim, nemotron | NVIDIA_API_KEY | https://integrate.api.nvidia.com/v1 | chat_completions | api_key |
| `ollama-cloud` | ollama_cloud | OLLAMA_API_KEY | https://ollama.com/v1 | chat_completions | api_key / oauth |
| `stepfun` | step, stepfun-coding-plan | STEPFUN_API_KEY | https://api.stepfun.ai/step_plan/v1 | chat_completions | api_key |
| `meta-ai` | meta, muse, muse-spark, model-api | MODEL_API_KEY, META_API_KEY, META_MODEL_API_KEY | https://api.meta.ai/v1 | responses | api_key |
| `deepinfra` | deep-infra | DEEPINFRA_API_KEY | https://api.deepinfra.com/v1/openai | chat_completions | api_key |
| `upstage` | solar | UPSTAGE_API_KEY | https://api.upstage.ai/v1 | chat_completions | api_key |
| `lmstudio` | lm-studio, lm_studio | LM_API_KEY (optional) | http://127.0.0.1:1234/v1 | chat_completions | none unless a key is stored |
| `ollama` | (none) | (none) | http://127.0.0.1:11434/v1 | chat_completions | none |
| `llamacpp` | llama.cpp, llama-cpp | (none) | resolved by `local-models` | chat_completions | none |
| `vllm`, `sglang` | (none) | (none) | required | chat_completions | none |
| `moa` | mixture-of-agents | (none) | `moa://local` (virtual, see `mixture-of-agents`) | aggregator's | none |
| `custom` | local | `providers.custom.key_secret` (optional) | required (`--base-url` or config) | chat_completions | api_key or none |
| `copilot`, `copilot-acp` (bridged `github-copilot` plugin) | github-copilot | COPILOT_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN | https://api.githubcopilot.com | chat_completions / external_process | oauth (device) |
| `solstice` (bridged plugin, hidden) | solstice-oauth | (sign-in) | https://generativelanguage.googleapis.com/v1alpha | gemini_native | oauth |

Each row's `key_hosts` SHALL equal the host of its default `base_url` (plus the auto-detected alternatives for `zai` and `kimi-coding`), the `actual` provider SHALL use auth `none` only when its configured `base_url` is loopback, and providers whose descriptor sets `supports_model_listing = true` (including `router`, `fireworks` and `gmi`, whose listings are account-scoped) SHALL populate the picker from their live listing.
**Milestone:** M1

#### Scenario: Plan key preferred over shared key
- **GIVEN** secrets `ALIBABA_CODING_PLAN_API_KEY` and `DASHSCOPE_API_KEY` both present
- **WHEN** provider `alibaba-coding-plan` resolves its credential
- **THEN** the system SHALL use `ALIBABA_CODING_PLAN_API_KEY` and `prometheus model explain` SHALL name it as the source

#### Scenario: Shared key fallback
- **GIVEN** only a `DASHSCOPE_API_KEY` secret
- **WHEN** provider `alibaba-coding-plan` resolves its credential
- **THEN** the system SHALL use `DASHSCOPE_API_KEY` against `coding-intl.dashscope.aliyuncs.com`

#### Scenario: Every built-in id listed
- **WHEN** the user runs `prometheus model list --providers --json`
- **THEN** the output SHALL contain every canonical id of the table above with its aliases, wire format, default base URL and secret names, and no secret value

### Requirement: Endpoint auto-detection for multi-region key providers
The system SHALL, for `zai` when no `base_url` is configured and a key is present, probe in parallel the endpoints `https://api.z.ai/api/paas/v4`, `https://open.bigmodel.cn/api/paas/v4`, `https://api.z.ai/api/coding/paas/v4` and `https://open.bigmodel.cn/api/coding/paas/v4` with a 1-token chat request (8-second timeout per probe), selecting the highest-priority endpoint in that order that answered HTTP 200 once every higher-priority probe has finished, persisting the winner in profile state keyed by a SHA-256 prefix of the key (never the key), and caching a failed probe set for 300 seconds before re-probing; and for `kimi-coding` SHALL route a key beginning `sk-kimi-` to `https://api.kimi.com/coding` over `anthropic_messages` and any other key to the default Moonshot endpoint; an explicit `base_url` SHALL always win over detection.
**Milestone:** M2

#### Scenario: Coding-plan key lands on the coding endpoint
- **GIVEN** a `GLM_API_KEY` accepted only by `https://api.z.ai/api/coding/paas/v4`
- **WHEN** the first session on `zai` resolves its endpoint
- **THEN** the system SHALL select the coding endpoint, persist it, and SHALL NOT probe again on the next session start

#### Scenario: Failed probes are not repeated immediately
- **GIVEN** a `zai` key rejected by all four endpoints
- **WHEN** the model picker opens twice within 300 seconds
- **THEN** the system SHALL run the probe set only once and use the default endpoint

### Requirement: Session affinity headers and per-model wire routing
The system SHALL support a descriptor field `session_affinity_header` naming an opaque per-conversation header (`x-opencode-session` for `opencode-zen` and `opencode-go`) whose value is a stable hash of the root session id (or of the kanban task id for kanban workers) and is sent on main and auxiliary calls of that conversation, and SHALL support a descriptor `model_wire_formats` table mapping model id patterns to wire formats (for `opencode-go`: MiniMax and Qwen models over `anthropic_messages`, others over `chat_completions`), re-applied whenever a session is resumed or switches model, with a model id ending `-vision` implying `supports_vision = true`.
**Milestone:** M2

#### Scenario: Affinity header stable within a conversation
- **GIVEN** a session on `opencode-zen` that makes a main call and a title auxiliary call
- **WHEN** both requests are built
- **THEN** both SHALL carry the same `x-opencode-session` value and a different session SHALL carry a different value

#### Scenario: Per-model wire format on resume
- **GIVEN** a resumed `opencode-go` session on a MiniMax model
- **WHEN** its next request is built
- **THEN** the system SHALL use `anthropic_messages`

### Requirement: Gemini native and AWS Bedrock providers
The system SHALL ship first-party provider plugins, bundled and enabled by default, implementing the `gemini_native` wire (Google AI Studio `generateContent`/`streamGenerateContent`, mapping `reasoning_effort` to the model's thinking level or budget and omitting it for models without thinking) and the `bedrock_converse` wire (Converse/ConverseStream signed with SigV4 from the AWS credential chain — environment secrets, shared profile `providers.bedrock.aws_profile`, SSO, instance or container role — in region `providers.bedrock.region`, default `us-east-1`), where `bedrock` lists foundation models and inference profiles through the Bedrock control-plane API, caches each model's context window for `providers.bedrock.catalog_ttl_secs` (default 86400), and reports credential-chain and region diagnostics in `prometheus doctor`; a `gemini` `base_url` ending `/openai` SHALL select `chat_completions` instead.
**Milestone:** M2

#### Scenario: Bedrock inference profile selected
- **GIVEN** valid AWS SSO credentials and `providers.bedrock.region = "eu-west-1"`
- **WHEN** the user runs `prometheus model list --provider bedrock`
- **THEN** the output SHALL include the region's inference profiles and foundation models with their context windows

#### Scenario: No AWS credentials
- **GIVEN** no AWS credentials in any chain source
- **WHEN** a session starts on `bedrock`
- **THEN** the system SHALL fail with `provider.missing_credential` naming the AWS credential chain and exit a non-interactive command with status 78

### Requirement: Azure AI Foundry provider
The system SHALL support `azure-foundry` with a required per-resource `providers.azure-foundry.base_url`, authenticating with `AZURE_FOUNDRY_API_KEY` (`api-key` header) or, when `providers.azure-foundry.auth = "entra_id"`, with a Microsoft Entra ID bearer token minted from the Azure CLI login or a managed identity and refreshed 60 seconds before expiry, selecting `anthropic_messages` for deployments serving Claude models and `responses` or `chat_completions` otherwise from the deployment's metadata, and listing deployments through the resource's deployment endpoint in the model picker.
**Milestone:** M2

#### Scenario: Keyless Entra ID access
- **GIVEN** `auth = "entra_id"`, an active `az login`, and no `AZURE_FOUNDRY_API_KEY`
- **WHEN** a request is sent
- **THEN** the system SHALL attach a minted Entra ID bearer token and record `billing_source = "api_key:azure-foundry/minted"`

#### Scenario: Claude deployment uses Anthropic wire
- **GIVEN** a Foundry deployment serving a Claude model
- **WHEN** the wire format is selected
- **THEN** the system SHALL select `anthropic_messages`

### Requirement: Data-use confirmation for training-flagged models
The system SHALL support model metadata flag `trains_on_data` (set in the built-in catalog for `meta-ai` contributor-tier models such as `muse-spark-*-contributor`), and SHALL require an explicit confirmation naming the data-use terms before such a model is selected in the picker, by `/model`, by `prometheus model set`, or as a fallback, auxiliary or MoA slot, failing a non-interactive selection with exit 4 unless `[models."<provider>/<model>"] accept_data_use = true` is set.
**Milestone:** M2

#### Scenario: Non-interactive selection refused
- **GIVEN** no `accept_data_use` for `meta-ai/muse-spark-1.2-contributor`
- **WHEN** the user runs `prometheus model set meta-ai/muse-spark-1.2-contributor` without a terminal
- **THEN** the system SHALL exit with status 4 naming the data-use terms and SHALL NOT modify `config.toml`

### Requirement: Hermes model-provider plugins hosted by the Python bridge
The system SHALL host Hermes model-provider plugins (for example `github-copilot` providing `copilot` and `copilot-acp`, and `solstice`) through the Python bridge as provider-kind plugins whose descriptors join the profile registry like any other plugin provider, running their OAuth sign-in through the host's common flows (Copilot: GitHub device-code sign-in, then exchange of the GitHub token for a short-lived Copilot API token refreshed before expiry; secrets `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN` accepted in that order) so the bridged plugin receives only an opaque credential handle, and SHALL fail with `provider.plugin_unavailable` naming the plugin when the bridge or the plugin is not installed, offering `prometheus plugins install <name>`.
**Milestone:** M2

#### Scenario: Copilot through the bridge
- **GIVEN** the `github-copilot` plugin installed and a completed `prometheus auth add copilot` device-code sign-in
- **WHEN** a session runs on `copilot`
- **THEN** the system SHALL route the call through the bridged provider with a short-lived Copilot token and label it `subscription:copilot/<label>`

#### Scenario: Profile references a missing bridged provider
- **GIVEN** `model.provider = "copilot"` and the plugin not installed
- **WHEN** a session starts
- **THEN** the system SHALL fail with `provider.plugin_unavailable` suggesting `prometheus plugins install github-copilot` and exit a non-interactive command with status 78

### Requirement: Local server detection and context probes
The system SHALL, for local-server providers, normalise a configured `base_url` that lacks a `/v1` path by trying both the given root and `<root>/v1` and persisting whichever served the model listing; list models through the server's native API when available (LM Studio `/api/v1/models`, Ollama `/api/tags`, OpenAI-compatible `/v1/models`); probe the served context window from Ollama `/api/show` (honouring the model's `num_ctx`), llama.cpp `/props` (`n_ctx`) and vLLM/SGLang `/v1/models` (`max_model_len`); warn once per session when an Ollama model runs with `num_ctx` of 2048 or less, naming the `num_ctx` remedy; and warn when the local model's metadata reports no tool-calling support.
**Milestone:** M1

#### Scenario: llama.cpp context from /props
- **GIVEN** a `llamacpp` endpoint whose `/props` reports `n_ctx = 65536`
- **WHEN** model metadata resolves without a config override
- **THEN** the context window SHALL be 65536

#### Scenario: Root URL normalised
- **GIVEN** `providers.lmstudio.base_url = "http://127.0.0.1:1234"`
- **WHEN** the endpoint is tested
- **THEN** the system SHALL try `http://127.0.0.1:1234/v1`, persist it when it serves the model listing, and send chat requests to the same prefix

#### Scenario: Small Ollama context warned
- **GIVEN** an Ollama model running with `num_ctx = 2048`
- **WHEN** a session starts on it
- **THEN** the system SHALL warn naming `num_ctx` before the `model.min_context_window` check applies

### Requirement: WSL2 local endpoint diagnostics
The system SHALL, in `prometheus doctor` and in the `provider.missing_base_url`/network error for a loopback local provider, detect running under WSL2 with NAT networking and, when the loopback endpoint is unreachable, suggest enabling WSL mirrored networking or using the Windows host address from the default route, and SHALL report this as a WARN check rather than a failure.
**Milestone:** M2

#### Scenario: Windows-hosted Ollama unreachable from WSL2
- **GIVEN** Prometheus running under WSL2 NAT and Ollama listening only on the Windows host
- **WHEN** the user runs `prometheus doctor`
- **THEN** the output SHALL contain a WARN naming mirrored networking and the detected host IP

### Requirement: Context length detection chain
The system SHALL resolve a model's context window in order from: the `[models."<provider>/<model>"] context_window` pin (shown with a `(pinned)` label in `/model`, status and picker); the provider's per-model listing; a local server probe; a persistent discovered-value cache in `state.db` (keyed by provider, model and base URL, reused across sessions for `providers.<id>.catalog_ttl_secs`); the built-in catalog; the bundled models.dev snapshot refreshed by `prometheus model list --refresh`; family defaults by model-name prefix; and finally 128000 tokens, recording the winning source in `prometheus model explain`.
**Milestone:** M1

#### Scenario: Pinned window labelled
- **GIVEN** `[models."openrouter/x"] context_window = 200000`
- **WHEN** the status line renders
- **THEN** the context gauge SHALL show `200000 (pinned)`

#### Scenario: Discovered value reused offline
- **GIVEN** a model whose window was discovered from the provider listing yesterday
- **WHEN** a session starts while the listing endpoint is unreachable
- **THEN** the system SHALL use the cached window without warning

### Requirement: Reasoning effort defaults, overrides and display
The system SHALL accept reasoning levels `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` and `ultra` with `agent.reasoning_effort` (default empty = `medium`), SHALL apply `[agent.reasoning_overrides]` entries keyed by model id matched case-insensitively ignoring `-`, `_`, `.` and a provider prefix, each value either a level or a provider-specific tier name mapped through `[agent.reasoning_tiers]` (for example `ultra = "max"`), SHALL clamp a requested level downward to the nearest level the route's catalog metadata supports (never upward), SHALL omit reasoning controls on routes reported as without reasoning, and SHALL provide `/reasoning [<level>|show|hide|full|clamp] [--global]` where a level sets the session effort (persisted with `--global`), `show`/`hide` toggle `display.show_reasoning`, and `full`/`clamp` set `display.reasoning_full`; an effort change SHALL apply from the next request without modifying the system prompt.
**Milestone:** M1

#### Scenario: Override matched despite spelling
- **GIVEN** `[agent.reasoning_overrides] "gpt-5.5" = "high"`
- **WHEN** a session runs `openrouter/openai/gpt_5_5`
- **THEN** the requests SHALL carry effort `high`

#### Scenario: Clamp downward
- **GIVEN** a route whose catalog supports efforts up to `high`
- **WHEN** the user runs `/reasoning ultra`
- **THEN** the system SHALL send `high` and report the clamp

### Requirement: Fast mode service tier
The system SHALL provide `/fast [normal|fast|auto|cold|ultrafast|status] [--global]` and `agent.fast_mode` (default `normal`) where `fast` requests the provider's priority tier on every request, `ultrafast` requests the provider's fastest tier where one exists (otherwise behaving as `fast`), `auto` only on requests within the first `agent.fast_auto_secs` (default 60) of each turn, and `cold` that same window on the first turn of a session only, mapping the tier to OpenAI priority processing and Anthropic fast mode and ignoring it with a one-time notice on routes without such a tier; each ledger row SHALL record `service_tier` so its price is accounted.
**Milestone:** M2

#### Scenario: Auto window
- **GIVEN** `/fast auto` and a turn whose tool loop runs 3 minutes
- **WHEN** requests are sent at 10 s and at 90 s into the turn
- **THEN** only the first SHALL request the priority tier

### Requirement: Credential pool maintenance commands
The system SHALL provide `prometheus auth priority <provider> <label|index> <position>` (0-based, renumbering the remaining entries, defining `fill_first` order), `prometheus auth add ... --priority <n>`, `prometheus auth reset <provider> [<label|index>]` (clearing benches and cooldowns, picked up by running sessions on their next request), `prometheus auth refresh <provider> [<label|index>]` (forcing an OAuth refresh and returning the credential to rotation), `prometheus auth logout <provider>` (removing every subscription credential of that provider), and an interactive manager when `prometheus auth` runs with no subcommand on a terminal; `auth list` and `auth status` SHALL be peeks that never refresh, rotate, or bench a credential.
**Milestone:** M2

#### Scenario: Reset reaches a running session
- **GIVEN** a gateway session whose only `openrouter` credential is benched for 3600 seconds
- **WHEN** the user runs `prometheus auth reset openrouter`
- **THEN** the session's next request SHALL use that credential without restarting the gateway

#### Scenario: Status is a peek
- **GIVEN** an OAuth credential expiring in 30 seconds
- **WHEN** the user runs `prometheus auth status openai-codex`
- **THEN** the system SHALL report the expiry and SHALL NOT send a refresh request

### Requirement: Credential pool seeding, custom endpoint pools and child sharing
The system SHALL seed a provider's pool from every secret named in its `secret_names` plus numbered siblings `<NAME>_<n>` (n from 2 upward, e.g. `OPENROUTER_API_KEY_2`) with source `seeded`, SHALL prune a seeded member when its secret disappears while never pruning members added by `prometheus auth add`, SHALL support pools for `[providers.<custom>]` endpoints, and SHALL let delegated children and auxiliary calls that resolve to the same provider draw from the parent's pool with shared bench and cooldown state in `state.db`.
**Milestone:** M2

#### Scenario: Numbered siblings join the pool
- **GIVEN** secrets `OPENROUTER_API_KEY` and `OPENROUTER_API_KEY_2`
- **WHEN** `prometheus auth list --json` runs
- **THEN** the `openrouter` pool SHALL contain two `seeded` members

#### Scenario: Child sees parent's bench
- **GIVEN** a parent session that benched pool member #1 after HTTP 429
- **WHEN** a delegated child on the same provider sends a request
- **THEN** the child SHALL skip member #1

### Requirement: External agent CLI executable resolution
The system SHALL resolve the executable of an `external_process` provider (and of the Codex app-server runtime) from `providers.<id>.command_path` (or `model.codex_bin`) when set, as a single absolute executable path never parsed as a shell command, otherwise through the platform executable lookup searching, in order, `PATH`, the user-local bin directories and the vendor's documented install prefixes, so that gateway services, cron runs and desktop launches with a minimal `PATH` find the same CLI as an interactive shell.
**Milestone:** M2

#### Scenario: Service launch with bare PATH
- **GIVEN** a gateway service whose `PATH` is `/usr/bin:/bin` and Claude Code installed in the user-local bin directory
- **WHEN** an `external_process` provider using `claude` starts
- **THEN** the system SHALL find and run the user-local executable

### Requirement: Auxiliary concurrency and request overrides
The system SHALL cap concurrently in-flight auxiliary calls per profile at `auxiliary.max_concurrency` (default 8, minimum 1), additionally honouring an optional per-task `[auxiliary.<task>] max_concurrency`, queueing excess calls, and SHALL merge `[auxiliary.<task>] extra_body` (a table) into that task's request body after the wire-format mapping, so an OpenRouter routing block or `plugins` entry can be set for one task only.
**Milestone:** M2

#### Scenario: Compression concurrency capped
- **GIVEN** `[auxiliary.compression] max_concurrency = 2` and five sessions compressing at once
- **WHEN** their compression calls are issued
- **THEN** at most two SHALL be in flight and the rest SHALL wait

### Requirement: OpenRouter provider routing preferences
The system SHALL map an optional `[provider_routing]` table (`sort` = `price`|`throughput`|`latency`, `only`, `ignore`, `order`, `require_parameters`, `data_collection` = `allow`|`deny`) to the OpenRouter request `provider` object only for routes on `openrouter`, SHALL apply per-model overrides from `[provider_routing.models."<model>"]` matched case-insensitively ignoring separators and following the current route (including fallback entries, cron jobs and delegated children), SHALL translate the model suffixes `:nitro` (sort by throughput) and `:floor` (sort by price), and SHALL send `plugins = [{id = "pareto-router", min_coding_score = <v>}]` with `openrouter.min_coding_score` (default 0.65, range 0.0-1.0) only when the model is `openrouter/pareto-code`, omitting the block when the value is empty.
**Milestone:** M2

#### Scenario: Routing ignored for other providers
- **GIVEN** `[provider_routing] only = ["anthropic"]` and a session on `anthropic` directly
- **WHEN** the request is built
- **THEN** no `provider` object SHALL be sent

#### Scenario: Pareto score sent only for the router model
- **GIVEN** `openrouter.min_coding_score = 0.8`
- **WHEN** requests are built for `openrouter/pareto-code` and for `openrouter/anthropic/claude-x`
- **THEN** only the first SHALL carry the `pareto-router` plugin block with `min_coding_score = 0.8`

### Requirement: Codex app-server runtime
The system SHALL support an opt-in `model.openai_runtime` (`auto` default, or `codex_app_server`) under which eligible turns on `openai-codex`, `openai-api` and custom providers also defined in the Codex config are handed to a long-lived `codex app-server` subprocess (executable from `model.codex_bin`, default `codex` via executable lookup), sending the persona once as developer instructions, mapping Prometheus approval requests to `approval.request`, and projecting Codex events (messages, tool calls, patches) into the session transcript and trajectory so memory and skill nudges still fire; `/codex-runtime [auto|codex_app_server|on|off]` and `prometheus codex-runtime migrate [--dry-run] [--json]` SHALL change the runtime effective from the next session only, refusing with an install hint when the executable is missing.
**Milestone:** M3

#### Scenario: Runtime switch deferred
- **GIVEN** a running session on `openai-codex` with `openai_runtime = "auto"`
- **WHEN** the user runs `/codex-runtime on`
- **THEN** the current session SHALL keep its runtime and the next new session SHALL use the app-server

#### Scenario: Codex binary missing
- **GIVEN** no `codex` executable resolvable
- **WHEN** the user runs `/codex-runtime on`
- **THEN** the system SHALL refuse with an install hint naming `model.codex_bin` and leave the config unchanged

### Requirement: Codex runtime migration
The system SHALL, when enabling the Codex app-server runtime or running `prometheus codex-runtime migrate`, write a delimited managed section into `~/.codex/config.toml` (preserving all user-authored text outside it, replacing the section idempotently) that translates the profile's MCP servers (stdio command/args/env to stdio, URL/headers to streamable HTTP, timeouts to `tool_timeout_sec`/`startup_timeout_sec`, disabled flags kept, unsupported keys dropped with a per-server warning, secret values referenced rather than inlined when Codex supports env references), enables installed Codex plugins reported `installed` and available by Codex's `plugin/list`, and registers `prometheus mcp serve --stdio` exposing `web_search`, `web_extract`, `vision_analyze`, `image_generate`, `skills_list`, `skill_view` and `text_to_speech` as a callback server; `--json` SHALL print `{migrated, preserved_user_servers, skipped_keys_per_server, errors, target_path, written}` and `--dry-run` SHALL write nothing.
**Milestone:** M3

#### Scenario: User text preserved
- **GIVEN** a `~/.codex/config.toml` with a user-authored `[mcp_servers.mine]` block outside the managed section
- **WHEN** migration runs twice
- **THEN** the user block SHALL be unchanged and the managed section SHALL appear exactly once

### Requirement: Launch-only ad-hoc endpoint
The system SHALL accept `prometheus chat --provider custom --base-url <URL> [--api-key-stdin]` for one invocation, reading the key from stdin (never argv), holding it in memory only, never writing it to the secrets store, config, trajectory or logs, and binding it to the given URL's host only.
**Milestone:** M1

#### Scenario: Ad-hoc key never persisted
- **GIVEN** `echo $KEY | prometheus chat --provider custom --base-url https://llm.example.com/v1 --api-key-stdin -z hi`
- **WHEN** the run completes
- **THEN** no file under the Prometheus home SHALL contain the key

### Requirement: User model aliases
The system SHALL accept `[model.aliases.<name>]` entries, either a `"<provider>/<model>"` string or a table with `provider`, `model`, optional `base_url` and optional `key_secret` (a secret name), usable wherever a model is named (`/model`, `prometheus model set`, fallback, auxiliary and MoA slots), matched case-insensitively and shadowing built-in short names, and SHALL never carry a credential from the previously active provider to an alias's route.
**Milestone:** M1

#### Scenario: Alias switches provider and key
- **GIVEN** `[model.aliases.fav] provider = "openrouter"`, `model = "anthropic/claude-x"` and a session on `nous`
- **WHEN** the user runs `/model FAV`
- **THEN** the route SHALL become `openrouter/anthropic/claude-x` with an OpenRouter credential and no Nous token

### Requirement: Fallback chain commands
The system SHALL provide `prometheus fallback list [--json]`, `prometheus fallback add [<provider>/<model>]` (the same interactive picker as `prometheus model` when no argument is given, validating credentials and capabilities before writing), `prometheus fallback remove <index|provider/model>` and `prometheus fallback clear`, each editing `[[fallback.chain]]` in `config.toml` and taking effect for new sessions.
**Milestone:** M1

#### Scenario: Duplicate entry refused
- **GIVEN** a chain containing `openrouter/x`
- **WHEN** the user runs `prometheus fallback add or/x`
- **THEN** the system SHALL report the entry already exists and leave the chain unchanged

### Requirement: Account usage windows command
The system SHALL provide `prometheus auth usage [--provider ID] [--json]` printing the account usage of the configured (or given) provider without starting a session, with the JSON document `{provider, source, title, plan, fetched_at, windows: [{label, used_percent, resets_at, detail}], details, unavailable_reason}` (keys stable, only extended by addition), exiting 1 with a message naming the provider when no credential, usage endpoint or fetch succeeds; `/usage` SHALL include the same account block plus session duration and current context window usage, and `/usage reset [--force]` SHALL redeem a banked limit reset offered by the provider (ChatGPT plan) after a confirmation that `--force` skips.
**Milestone:** M2

#### Scenario: JSON for scripts
- **GIVEN** a ChatGPT sign-in with 5-hour and weekly windows
- **WHEN** the user runs `prometheus auth usage --provider openai-codex --json`
- **THEN** stdout SHALL be one JSON document with two `windows` entries carrying `used_percent` and `resets_at`

#### Scenario: Nothing to report
- **GIVEN** provider `deepseek` with no usage endpoint
- **WHEN** the user runs `prometheus auth usage --provider deepseek`
- **THEN** the system SHALL print the unavailable message to stderr and exit with status 1

### Requirement: Subscription account commands
The system SHALL provide in-chat `/login [provider]` (default `nous`) running the device-code sign-in off-turn without blocking the session and, on the gateway, only in a paired direct message (refused in groups and channels), `/subscription` (alias `/upgrade`) showing the signed-in subscription provider's plan label and opening (or printing) its plan-management URL from the descriptor's `account_urls.plan`, and `/topup` showing the prepaid balance where the provider reports one and the descriptor's `account_urls.billing`; providers without these URLs SHALL answer `not available for <provider>`.
**Milestone:** M2

#### Scenario: Login refused in a group
- **GIVEN** a Telegram group chat
- **WHEN** a user sends `/login`
- **THEN** the system SHALL refuse and tell the user to DM the bot

### Requirement: Subscription tool gateway routing
The system SHALL route a tool category (web search, web extract, image generation, video generation, TTS, STT, cloud browser, managed remote sandbox) through a subscription provider's tool gateway when that category's single selection key, owned by the tool's capability (`web.search_backend` and `web.extract_backend` falling back to `web.backend`, `image_gen.provider`, `video_gen.provider`, `tts.provider`, `stt.provider`, `browser.provider`, `terminal.backend`) is set to the provider id (`nous`), SHALL always honour a stored selection regardless of which secrets exist (a missing vendor key fails with a message naming the selection, never silently rerouting), SHALL autodetect only for categories never configured, SHALL accept `[tool_gateway] domain`, `scheme` and per-label `urls.<label>` for a self-hosted gateway (with the gateway user token kept in the secrets store), and SHALL fail a gateway-routed call after subscription expiry with `auth.plan_not_eligible` naming the portal; `prometheus portal status|open|tools` and `prometheus setup --portal` (sign in, set the main provider to `nous`, offer to route every category through the gateway) SHALL show and change this routing.
**Milestone:** M2

#### Scenario: Stored selection wins over a present key
- **GIVEN** `image_gen.provider = "nous"` and a `FAL_KEY` secret
- **WHEN** image generation runs
- **THEN** the call SHALL go through the Nous tool gateway with `billing_source = "subscription:nous/<label>"`

#### Scenario: Selected vendor without key
- **GIVEN** `image_gen.provider = "fal"` and no `FAL_KEY`
- **WHEN** image generation runs
- **THEN** the tool SHALL fail naming `image_gen.provider` and SHALL NOT use the gateway

### Requirement: Outbound IPv4 preference
The system SHALL race IPv6 and IPv4 connection attempts for every outbound connection it makes (provider calls, adapters, downloads), starting IPv4 250 ms after IPv6, except that `network.force_ipv4` and the IPv6 doctor check are defined once by `workspace-architecture` › IPv6 reachability check and IPv4-only outbound and, when that key is true, no IPv6 attempt SHALL be made.
**Milestone:** M2

#### Scenario: Blackholed IPv6 falls back quickly
- **GIVEN** `network.force_ipv4 = false` and a dual-stack provider host whose IPv6 address never answers
- **WHEN** a provider request is made
- **THEN** the IPv4 attempt SHALL start 250 ms after the IPv6 attempt and the request SHALL succeed over IPv4
