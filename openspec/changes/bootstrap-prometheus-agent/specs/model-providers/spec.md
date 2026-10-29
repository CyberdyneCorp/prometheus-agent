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
The system SHALL fail with code `provider.missing_base_url` when a local-server provider (for example `ollama`, `vllm`, `llamacpp`, `lmstudio`) has no configured `base_url`, and SHALL fail with code `provider.missing_credential` when a provider whose `auth` is not `none` has no resolvable credential, and in neither case SHALL route the request to a different provider unless that provider is an explicit entry of the fallback chain.
**Milestone:** M1

#### Scenario: Ollama without an endpoint
- **GIVEN** `--provider ollama`, no `providers.ollama.base_url`, and an `OPENROUTER_API_KEY` secret
- **WHEN** the route resolves
- **THEN** the system SHALL fail with `provider.missing_base_url` and SHALL send no request to OpenRouter

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
The system SHALL support OAuth and subscription sign-in for every provider whose descriptor declares `auth = "oauth"` or `auth = "token_mint"`, shipping at least `anthropic` (Claude Max), `openai-codex` (ChatGPT plan), `xai-oauth` (SuperGrok / X Premium+), `nous` (Nous Portal), `qwen-oauth` (Qwen Portal), `minimax-oauth` (MiniMax portal), `openrouter` (OAuth key minting) and `vertex` (Google Cloud), SHALL let one provider hold API-key and subscription credentials side by side, choosing between them by `providers.<id>.prefer_credential` (`subscription` default, or `api_key`), and SHALL show the descriptor's `plan_notes` (eligibility and billing caveats, data in the provider descriptor or `[providers.<id>] plan_notes`, never text hardcoded in sign-in code) before every sign-in starts and in `prometheus auth status <provider>`.
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
- **GIVEN** a valid `nous` sign-in and `[tools.web_search] provider = "nous"`
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
The system SHALL label every model call with a `billing_source` of `subscription:<provider>/<label>`, `api_key:<provider>/<label>` or `local`, recording it on the `model.call` trajectory event, the trajectory and the usage ledger, SHALL show the current session's billing source in the CLI status line, TUI, desktop and dashboard, and SHALL state the billing change in the `provider.fallback` notice (for example `now billing API key openrouter/default instead of subscription openai-codex/personal`) whenever a route change moves between a subscription and a pay-per-token credential; when `providers.<id>.confirm_metered_fallback = true`, an interactive session SHALL ask before making such a move and a non-interactive one SHALL fail the turn instead.
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
The system SHALL resolve a model's metadata (`context_window`, `max_output_tokens`, `supports_tools`, `supports_vision`, `supports_reasoning`, per-million-token input/output/cache prices) in order from: `[models."<provider>/<model>"]` overrides in `config.toml`; the provider's live model listing cached for `providers.<id>.catalog_ttl_secs` (default 86400); the built-in catalog shipped with the binary; and otherwise defaults of a 128000-token context window, tools supported, vision unsupported and unknown prices, logging one WARN per (provider, model) per process naming the override key.
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
The system SHALL route side-model calls by a typed auxiliary task enum (`compression`, `title`, `vision`, `approval`, `memory`, `curator`, `web_extract`, `skill_review`, `evolution`), reading `[auxiliary.<task>]` keys `provider`, `model`, `timeout_secs` and `fallback_chain` with precedence explicit call argument over config over `auto`, SHALL fail config validation for an unknown task name, and SHALL default timeouts to 120 seconds for `vision` and `compression`, 30 seconds for `approval` and `title`, and 600 seconds for `curator` and `evolution`. It SHALL retry a retryable auxiliary failure on the same route `auxiliary.transient_retries` times (default 2, range 0-6), SHALL emit `aux_call.start` and `aux_call.end` events once per attempt and never the main-loop `model.call` event for auxiliary calls, and SHALL record auxiliary cost in the usage ledger under the owning session with `kind = "auxiliary"` and the task name.
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
