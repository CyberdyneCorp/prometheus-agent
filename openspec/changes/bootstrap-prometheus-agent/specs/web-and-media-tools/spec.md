# Web and Media Tools

## Purpose
Web and media tools are the model-callable tools that reach outside the workspace for information or produce and consume media: `web_search`, `web_extract`, `x_search`, `vision_analyze`, `video_analyze`, `image_generate`, `video_generate`, provider-contributed video tools, `text_to_speech`, and the speech-to-text service used by voice input. The key decision is that every tool has one fixed contract while backends are typed providers selected by explicit config keys (owned here: `web.*`, `vision.*`, `x_search.*`, `image_gen.*`, `video_gen.*`, `tts.*`, `stt.*`); an unknown or unavailable provider fails closed per `plugin-system`, and the only multi-vendor rotation (keyless web search) exists solely as explicit opt-in config, never as a silent fallback. In this spec `<profile>` denotes the profile's home directory resolved from the session's ProfileScope.

## ADDED Requirements

### Requirement: web_search tool contract
The system SHALL provide the `web_search` tool (toolsets `web` and `search`) with arguments `query` (string, required; passed verbatim to the backend so operators such as `site:`, `filetype:`, `intitle:`, `-term` and quoted phrases work where the backend supports them) and `limit` (integer, default 5, minimum 1, maximum 100), SHALL return `{"results": [{"title", "url", "snippet", "position"}], "backend": "<id>"}` ordered by backend rank with at most `limit` entries, and SHALL return `status = "error"` with `web_search failed (<backend>): <reason>` on backend failure, never an empty success; `web_search` SHALL be effect `none` and idempotent.
**Milestone:** M1

#### Scenario: Limit bounds the result list
- **GIVEN** a backend that returns 20 hits
- **WHEN** the model calls `web_search` with `{"query": "rust async", "limit": 3}`
- **THEN** the result SHALL contain exactly 3 entries with positions 1, 2 and 3

#### Scenario: Backend outage is an error
- **GIVEN** the configured search backend answers HTTP 503
- **WHEN** `web_search` is called
- **THEN** the result SHALL have `status = "error"` naming the backend and SHALL NOT be an empty result list

### Requirement: web_extract tool contract
The system SHALL provide the `web_extract` tool (toolset `web`) with arguments `urls` (array of 1 to 5 http(s) URLs, required) and `char_limit` (integer, optional, minimum 2,000), returning per URL `{url, title, content, status}` as Markdown with inline images rendered as `[IMAGE: <alt>]` placeholders and image links kept; the per-page budget SHALL be `char_limit` when given else `web.extract_char_limit` (default 15,000), clamped to 2,000–500,000; a page within budget SHALL be returned whole, and a longer page SHALL be returned as a deterministic window of about 75% head and 25% tail cut on line boundaries followed by a `[TRUNCATED]` footer naming the stored full-text path and the exact `read_file` call that pages through the omitted middle; no LLM summarization SHALL be applied. Each backend dispatch SHALL be bounded by `web.extract_timeout_secs` (default 120; 0 disables), yielding a per-URL timeout error rather than stalling, and one failing URL SHALL NOT fail the others.
**Milestone:** M1

#### Scenario: Long page is windowed deterministically
- **GIVEN** a page whose extracted Markdown is 60,000 characters and default config
- **WHEN** `web_extract` is called for it twice
- **THEN** both results SHALL be byte-identical, about 15,000 characters long, end with a `[TRUNCATED]` footer, and name the same full-text file

#### Scenario: Per-URL failure is isolated
- **GIVEN** two URLs where the second times out
- **WHEN** `web_extract` is called with both
- **THEN** the first entry SHALL carry its content and the second SHALL carry `status = "timeout"`

### Requirement: Extracted full text is stored and capped
The system SHALL store the clean full text of every truncated extraction under `<profile>/cache/web/` (mode 0600) keyed by URL, format and backend, SHALL cap a stored text at 2,000,000 characters with a final line stating the cap, and SHALL delete stored texts older than 7 days on daemon start and hourly thereafter.
**Milestone:** M1

#### Scenario: Huge page capped on disk
- **WHEN** an extraction yields 5,000,000 characters
- **THEN** the stored file SHALL hold the first 2,000,000 characters followed by the cap line

### Requirement: Built-in web backends with capability flags
The system SHALL ship web backends registered in the `plugin-system` provider registry with these ids and capabilities: `firecrawl` (search + extract; optional self-hosted base URL `web.firecrawl.base_url`), `searxng` (search only; requires `web.searxng.url`), `brave-free` (search only; secret `BRAVE_SEARCH_API_KEY`), `ddgs` (search only; keyless), `exa`, `parallel`, `tavily`, `keenable` (search + extract; secrets `EXA_API_KEY`, `PARALLEL_API_KEY`, `TAVILY_API_KEY`, `KEENABLE_API_KEY`), `perplexity` (search + query-relevant snippet extract; `PERPLEXITY_API_KEY`), `xai` (search only through the xAI Responses built-in web search; xAI API key or `xai-oauth` sign-in; options `web.xai.model`, `allowed_domains`/`excluded_domains` at most 5 and mutually exclusive, `timeout_secs` default 90), `openai-native` (search only; provider-executed web search on the OpenAI Responses route of an `openai-codex` sign-in, replacing the client-side `web_search` definition one-for-one only on that route) and `nous` (managed tool gateway; search + extract); selecting a search-only backend for extract SHALL make `web_extract` unavailable with `web_extract unavailable: backend '<id>' is search-only; set web.extract_backend`.
**Milestone:** M2

#### Scenario: Search-only backend for extract
- **GIVEN** `web.backend = "searxng"` and no `web.extract_backend`
- **WHEN** the session tool surface is built
- **THEN** `web_search` SHALL be present and `web_extract` SHALL be reported as `unavailable:backend 'searxng' is search-only; set web.extract_backend`

#### Scenario: openai-native on a non-Codex route
- **GIVEN** `web.search_backend = "openai-native"` and a main route on Anthropic Messages
- **WHEN** the session is built
- **THEN** `web_search` SHALL be reported unavailable with a reason naming the required `openai-codex` Responses route, and no other backend SHALL be substituted

### Requirement: Web backend selection precedence
The system SHALL resolve the backend for each web capability as `web.search_backend` or `web.extract_backend` (per capability) over `web.backend` (shared) over `auto` (default), where `auto` picks the first backend with credentials present in the secrets store in the fixed order `tavily`, `perplexity`, `exa`, `parallel`, `firecrawl`, `nous` (when a Nous sign-in with tool-gateway entitlement exists), `searxng` (URL configured), `brave-free`, then `keyless` only when `web.keyless.enabled = true`; `xai` and `openai-native` SHALL never be auto-selected; when nothing resolves, `web_search` and `web_extract` SHALL be unavailable with reason `no web backend configured`; the resolved backend per capability SHALL be shown by `prometheus tools explain web_search` and recorded in each result's `backend` field.
**Milestone:** M1

#### Scenario: Per-capability split
- **GIVEN** `web.search_backend = "searxng"` and `web.extract_backend = "firecrawl"`
- **WHEN** the model calls both tools
- **THEN** `web_search` SHALL report `backend = "searxng"` and `web_extract` SHALL report `backend = "firecrawl"`

#### Scenario: xAI key does not reroute web search
- **GIVEN** only an xAI API key in the secrets store and `web.backend = "auto"`
- **WHEN** the backend resolves
- **THEN** it SHALL NOT resolve to `xai` and the web tools SHALL be unavailable with `no web backend configured`

### Requirement: Keyless web search rotation is explicit opt-in
The system SHALL provide the backend id `keyless`, which rotates round-robin across the public free tiers of `exa`, `parallel`, `firecrawl` and `keenable` and moves to the next vendor on a rate-limit or refusal, and SHALL use it only when `web.backend`, `web.search_backend` or `web.extract_backend` names it, or `web.keyless.enabled = true` (default `false`) lets `auto` reach it; `web.keyless.rescue` (default `false`, effective only with `web.keyless.enabled`) SHALL retry exactly one failed call of a configured keyed backend once on the keyless ring without changing the backend used by the next call; per-vendor `web.provider_tier.<vendor>` in `free` or `paid` SHALL pin that vendor's anonymous or keyed endpoint (`paid` with no key is an error and removes the vendor from the ring); every keyless or rescued result SHALL carry `backend = "keyless:<vendor>"` and `rescued = true` when rescue was used, and keyless traffic SHALL be listed in `prometheus doctor` as outbound to third parties without an account.
**Milestone:** M2

#### Scenario: Default install does not rotate silently
- **GIVEN** a fresh profile with no web secrets and no web config
- **WHEN** the model surface is built
- **THEN** `web_search` SHALL be unavailable with `no web backend configured` and no request SHALL be sent to any keyless vendor

#### Scenario: Opt-in rescue is one-shot
- **GIVEN** `web.backend = "tavily"`, `web.keyless.enabled = true`, `web.keyless.rescue = true` and Tavily returning 401
- **WHEN** two searches run in sequence
- **THEN** each SHALL first try Tavily, each SHALL be answered by a keyless vendor with `rescued = true`, and neither SHALL make the next call skip Tavily

### Requirement: Web result caching
When `web.cache_enabled` is true (default) the system SHALL serve a repeat `web_search` with the same normalized query (case- and whitespace-insensitive), backend and limit bucket (limits rounded up to 10, 20, 50 or 100) from an in-process memo, and a repeat `web_extract` of the same URL, format and backend from the shared `<profile>/cache/web/` store visible to every process of the profile, both for `web.cache_ttl_secs` (default 1,200, clamped 60–86,400); concurrent identical searches SHALL be coalesced into one backend request; only successful responses SHALL be cached, each under the requested URL the backend reported for it; `localhost`, `*.local`, single-label hosts, loopback, private, link-local and CGNAT addresses and every host matching `web.cache_exempt_hosts` (exact, `*.` wildcard or domain suffix) SHALL never be cached; a cached result SHALL carry `cached = true`.
**Milestone:** M2

#### Scenario: Subagent fan-out shares one search
- **GIVEN** three subagents issuing the identical `web_search` at the same moment
- **WHEN** the calls run
- **THEN** exactly one backend request SHALL be sent and all three SHALL receive the same results

#### Scenario: Preview deploy stays live
- **GIVEN** `web.cache_exempt_hosts = ["*.ngrok-free.app"]`
- **WHEN** the same `https://abc.ngrok-free.app/` is extracted twice within the TTL
- **THEN** both calls SHALL hit the backend

### Requirement: x_search tool
The system SHALL provide `x_search` (toolset `x_search`, off by default) for read-only discovery on X through the xAI Responses built-in X search, with arguments `query` (required), `allowed_x_handles` or `excluded_x_handles` (each at most 10 handles, leading `@` stripped, mutually exclusive), `from_date` and `to_date` (`YYYY-MM-DD`, validated before any billable call: unparseable dates, `from_date` after `to_date` or a future `from_date` SHALL return `status = "error"` without contacting xAI), and `enable_image_understanding` / `enable_video_understanding` (default false); it SHALL use `x_search.model` (default `grok-4.5`), `x_search.reasoning_effort` (default unset), `x_search.timeout_secs` (default 180, minimum 30) and `x_search.retries` (default 2, on 5xx, read timeout or connection error with backoff of 1.5 × attempt seconds capped at 5), SHALL prefer an xAI API key over the `xai-oauth` subscription token at call time, SHALL be available only when one of them exists, SHALL return the answer with its citations, and SHALL set `status = "degraded"` with `degraded = true` when a narrowing filter was active and no citation came back.
**Milestone:** M2

#### Scenario: Future window refused locally
- **WHEN** the model calls `x_search` with `from_date` one week in the future
- **THEN** the result SHALL be an error stating `from_date (<date>) is in the future` and no request SHALL be sent

#### Scenario: Uncited filtered answer is degraded
- **GIVEN** `allowed_x_handles = ["someone"]`
- **WHEN** xAI answers without citations
- **THEN** the result SHALL have `status = "degraded"` and `degraded = true`

### Requirement: vision_analyze tool
The system SHALL provide `vision_analyze` (toolset `vision`) with arguments `image_url` (required; http(s) URL, local path resolved through the session's execution backend, or `data:` URL), `question` (required) and optional `region` `[x1, y1, x2, y2]` in original-image pixel coordinates applied before any downscaling; downloads SHALL be capped at 50 MiB and pass the `security-boundaries` URL policy; animated or multi-frame images SHALL be refused above 100 frames or 100,000,000 aggregate pixels; when the main route accepts images inside tool results the tool SHALL attach the (budgeted) image natively to its result, and otherwise SHALL ask the `vision` auxiliary task (`model-providers` › Auxiliary task routing) to describe the image and answer `question`, returning text; with no native path and no `vision` auxiliary route the tool SHALL be unavailable with reason `no vision-capable route`.
**Milestone:** M1

#### Scenario: Region crop keeps resolution
- **GIVEN** a 4000×3000 screenshot
- **WHEN** the model calls `vision_analyze` with `region = [0, 0, 800, 200]`
- **THEN** the analyzed image SHALL be the 800×200 crop at original resolution

#### Scenario: Text-only main model
- **GIVEN** a text-only main model and `[auxiliary.vision]` routed to a vision-capable model
- **WHEN** `vision_analyze` is called
- **THEN** the result SHALL be the auxiliary model's text description and SHALL NOT contain image parts

### Requirement: Image input routing
The system SHALL route every image that reaches the model — user attachments, `vision_analyze` results, browser screenshots, computer-use captures and MCP image results — by `vision.image_input_mode` in `auto` (default), `native` or `text`: `auto` SHALL attach pixels when the main route reports vision support and no explicit `[auxiliary.vision]` provider or model is configured, and otherwise replace the image with a `vision` auxiliary description; `native` SHALL always attach pixels; `text` SHALL never attach pixels; on routes that accept only JPEG, PNG, GIF and WebP inline, SVG and other non-raster images SHALL be rasterized to PNG before sending, or replaced by a text note naming the file when rasterization is impossible, so one image never makes the whole request fail. `vision_analyze` on a vision-capable main route whose wire format carries images in tool results SHALL return the image itself, and SHALL say the image is already in context when it is attached to the current user message without a `region`. The decision SHALL be taken when the message is first sent and SHALL NOT change for that message later in the session.
**Milestone:** M1

#### Scenario: Text mode keeps pixels out
- **GIVEN** `vision.image_input_mode = "text"` and a vision-capable main model
- **WHEN** the user attaches a photo
- **THEN** the request SHALL contain an auxiliary description of the photo and no image part

#### Scenario: SVG on a raster-only route
- **GIVEN** a Responses route accepting only raster images
- **WHEN** an inline SVG is attached
- **THEN** the request SHALL carry a PNG rasterization of it and SHALL be accepted

### Requirement: Image embed budget, per-image cap and replay
The system SHALL downscale any natively embedded image larger than `vision.embed_target_bytes` (default 262,144, clamped 65,536–4,194,304) or wider than 1,568 pixels to a JPEG that fits; SHALL allow the same image (region crops included; local paths compared by resolved path) to be embedded by `vision_analyze` at most `vision.max_calls_per_image` times per session (unset = 3 in delegated children and unlimited for the main agent; 0 = unlimited), afterwards returning `vision_analyze refused: this image has already been loaded into context <n> time(s)`; and on turns whose history is rebuilt from storage (resume, API continuation, daemon restart) SHALL re-send the newest `vision.replay_recent_images` (default 3; 0 disables) user images from their retained copies, in their original messages, byte-identical to their first send, retiring older images in batches so an earlier message is rewritten at most once per batch; images without a retained copy, sent as remote URLs or under `text` mode SHALL keep the `[image]` placeholder.
**Milestone:** M2

#### Scenario: Cap inside a subagent
- **GIVEN** a delegated child with `vision.max_calls_per_image` unset
- **WHEN** it calls `vision_analyze` on the same file a fourth time
- **THEN** the result SHALL be the refusal naming 3 previous loads

#### Scenario: Resume replays images
- **GIVEN** a session with 2 attached screenshots whose retained copies exist
- **WHEN** the session is resumed with `prometheus chat --resume <id>` on a vision-capable model
- **THEN** the next request SHALL include both images in their original user messages

### Requirement: video_analyze tool
The system SHALL provide `video_analyze` (toolset `video`, off by default) with arguments `video_url` (http(s) URL or local path, required) and `question` (required), accepting `.mp4`, `.webm`, `.mov`, `.avi`, `.mkv`, `.mpeg` and `.mpg`, warning in the result above 20 MiB and refusing above 50 MiB, and routing to the `vision` auxiliary task's route only when that route's model declares video input (`supports_video`), never to another route; with no video-capable route the tool SHALL be unavailable with reason `no video-capable route`.
**Milestone:** M2

#### Scenario: Oversized video
- **WHEN** `video_analyze` is called on a 70 MiB file
- **THEN** the result SHALL be an error stating the 50 MiB limit and no provider call SHALL be made

### Requirement: image_generate tool
The system SHALL provide `image_generate` (toolset `image_gen`) with arguments `prompt` (required), `aspect_ratio` in `landscape` (default), `square` or `portrait`, `upscale` (boolean, default false; honored only by backends with an upscaler, otherwise ignored and reported), and, only when the active model supports editing, `image_url` (source image) and `reference_image_urls` (capped at the model's reference limit); the tool description SHALL state at surface build whether editing is available for the configured model; image inputs passed to a text-only model SHALL fail with an error naming an edit-capable model; generated images SHALL be stored in the blob store and returned as `MEDIA:<path>` with MIME type, provider, model and the size actually produced; one tool-call batch SHALL run at most `image_gen.max_parallel_requests` (default 4, clamped to at least 1 and at most `agent.tool_concurrency`) generations concurrently; the agent SHALL NOT choose the model.
**Milestone:** M2

#### Scenario: Editing advertised only when supported
- **GIVEN** `image_gen.model` set to a text-to-image-only model
- **WHEN** the surface is built
- **THEN** the `image_generate` schema SHALL NOT contain `image_url` or `reference_image_urls`

#### Scenario: Result is media
- **WHEN** an image is generated
- **THEN** the result content SHALL include `MEDIA:<path>` pointing at a file in the profile blob store

### Requirement: Built-in image generation providers and models
The system SHALL ship image providers selected by `image_gen.provider` (default `fal`) and `image_gen.model`: `fal` (default model `fal-ai/flux-2/klein/9b`, catalog including `fal-ai/flux-2-pro`, `fal-ai/z-image/turbo`, `fal-ai/nano-banana-pro`, `fal-ai/gpt-image-1.5`, `fal-ai/gpt-image-2`, GPT Image 2.5 Flare and Sunburst, `fal-ai/ideogram/v3`, `fal-ai/recraft/v4/pro/text-to-image`, `fal-ai/qwen-image`, `fal-ai/krea/v2/medium/text-to-image`, `fal-ai/krea/v2/large/text-to-image`; GPT Image quality pinned to `medium`; secret `FAL_KEY`), `openai` (GPT Image models via an API key, with `image_gen.openai.base_url`, `image_gen.openai.key_secret` naming the secret to use, or `image_gen.openai.provider` naming a configured custom provider endpoint; catalog ids such as `gpt-image-2-medium` map to model plus quality, any other id is sent verbatim without quality), `openai-codex` (Codex sign-in image endpoints; results report requested and actually produced quality and size), `openrouter` (live image catalog), `xai`, `krea`, `meta-ai` (default `muse-image-1.0`), `deepinfra` and `nous` (managed tool gateway); each provider SHALL declare per model `supports_edit`, `max_reference_images` and `upscaler`, and a model id unknown to the chosen provider SHALL fail validation with exit 78 listing valid ids except for `openai` custom endpoints and `openrouter`.
**Milestone:** M2

#### Scenario: Default provider and model
- **GIVEN** a profile with `FAL_KEY` and no `image_gen` config
- **WHEN** `image_generate` runs
- **THEN** it SHALL use provider `fal` and model `fal-ai/flux-2/klein/9b`

#### Scenario: Custom OpenAI-compatible endpoint
- **GIVEN** `image_gen.provider = "openai"`, `image_gen.openai.base_url = "http://localhost:18081/v1"` and `image_gen.openai.model = "grok-imagine-image"`
- **WHEN** an image is generated
- **THEN** the request SHALL go to that base URL with `model = "grok-imagine-image"` and no `quality` field

### Requirement: video_generate tool
The system SHALL provide `video_generate` (toolset `video_gen`, off by default) with arguments `prompt` (required), `duration` (integer seconds; minimum and maximum advertised from the configured model's range), `aspect_ratio` (from `16:9` default, `9:16`, `1:1`, `4:3`, `3:4`, `3:2`, `2:3`, `21:9`, narrowed to the model's set) and `resolution` (from `480p`, `540p`, `720p` default, `768p`, `1080p`, narrowed), plus `image_url` (image-to-video), `reference_image_urls`, `negative_prompt`, `audio`, `seed` and `upscale` advertised only when the configured model honors them while the handler accepts all of them regardless (replay compatibility); providers `fal`, `xai`, `openrouter`, `deepinfra` and `nous` SHALL be selected by `video_gen.provider` and `video_gen.model` (never by the agent); jobs SHALL be polled every 5 seconds up to `video_gen.timeout_secs` (default 900) yielding `status = "timeout"` with the provider job id, and the finished video SHALL be downloaded into the blob store and returned as `MEDIA:<path>`.
**Milestone:** M3

#### Scenario: Stuck job times out
- **GIVEN** a provider job that never reaches a terminal state
- **WHEN** 900 seconds pass
- **THEN** the tool SHALL return `status = "timeout"` including the provider job id

### Requirement: Provider-contributed media tools
A media provider SHALL be able to contribute extra tools into its category's toolset that are registered only while that provider is the configured one and its credentials exist; the built-in `xai` video provider SHALL contribute `xai_video_edit` (`prompt`, `video_url`) and `xai_video_extend` (`prompt`, `video_url`, optional `duration`), where `video_url` must be the public HTTPS MP4 URL of a prior xAI result, and calls with any other URL SHALL return an error without contacting xAI.
**Milestone:** M3

#### Scenario: Tools absent for another provider
- **GIVEN** `video_gen.provider = "fal"`
- **WHEN** the surface is built with toolset `video_gen`
- **THEN** `video_generate` SHALL be present and `xai_video_edit` and `xai_video_extend` SHALL be absent

### Requirement: text_to_speech tool and delivery
The system SHALL provide `text_to_speech` (toolset `tts`) with arguments `text` (required), `output_path` (optional; must resolve inside the session workspace or `<profile>/cache/audio/`), `speed` (0.25–4.0, overriding the provider's configured speed) and `instructions` (style direction, honored by providers that support it); it SHALL split text longer than the provider's per-request cap into ordered sentence-aware chunks and concatenate the audio, never truncating silently; it SHALL return `MEDIA:<path>` plus `voice_compatible` (true when the audio is Opus/OGG or was converted to it); adapters that render voice bubbles (Telegram, Discord) SHALL receive Opus/OGG, converting MP3, WAV or PCM through the managed ffmpeg runtime when the provider does not emit Opus, and falling back to a plain audio attachment when conversion is impossible; on `cli` and `tui` the file SHALL be saved under `<profile>/cache/audio/` and its path shown.
**Milestone:** M2

#### Scenario: Long text is chunked
- **GIVEN** `tts.provider = "openai"` (cap 4,096 characters)
- **WHEN** `text_to_speech` is called with 10,000 characters
- **THEN** the provider SHALL receive 3 ordered requests and the result SHALL be one audio file covering all the text

#### Scenario: Telegram bubble
- **GIVEN** `tts.provider = "edge"` producing MP3 and a Telegram session
- **WHEN** the tool's media is delivered
- **THEN** the adapter SHALL send an Opus/OGG voice message

### Requirement: Built-in text-to-speech providers
The system SHALL ship TTS providers selected by `tts.provider` (default `edge`, keyless): `edge` (`tts.edge.voice` default `en-US-AriaNeural`; cap 5,000), `elevenlabs` (`voice_id` default `pNInz6obpgDQGcFmaJgB`, `model_id` default `eleven_multilingual_v2`; cap by model: 40,000 `eleven_flash_v2_5`, 30,000 `eleven_flash_v2`, 10,000 multilingual/english models and unknown models, 5,000 `eleven_v3`; the tool gains a `voice` argument and a voice lookup action so the agent can pick voices), `openai` (`gpt-4o-mini-tts`, voice `alloy`; cap 4,096; optional `base_url` and `consent_attestation`), `gemini` (`gemini-2.5-flash-preview-tts`, voice `Kore`; cap 32,000; optional `persona_prompt_file` with `{transcript}` placeholder and `audio_tags`), `xai` (voice `eve`, `language` `en`, `speed` 1.0 within 0.7–1.5, custom cloned voice ids; cap 15,000), `mistral` (`voxtral-mini-tts-2603`; cap 4,000), `minimax` (`speech-02-hd`; cap 10,000), `deepinfra`, `nous`, and local engines `piper` (`en_US-lessac-medium`, voices downloaded on first use; cap 5,000), `kittentts` (cap 2,000) and `neutts` (cap 2,000); `tts.<provider>.max_text_length` SHALL override a cap only when it is a positive integer (other values fail validation with exit 78); local engines SHALL stay loaded for `tts.keep_warm_secs` (default 60; 0 unloads at once) after the last surface releases its speech lease.
**Milestone:** M2

#### Scenario: Zero-config speech
- **GIVEN** a profile with no `tts` config and no TTS secrets
- **WHEN** `text_to_speech` is called
- **THEN** the system SHALL synthesize with provider `edge` and voice `en-US-AriaNeural`

#### Scenario: Invalid cap override
- **GIVEN** `tts.openai.max_text_length = 0`
- **WHEN** config is loaded
- **THEN** validation SHALL fail with exit 78 naming `tts.openai.max_text_length`

### Requirement: Built-in speech-to-text providers and resolution
The system SHALL ship STT providers selected by `stt.provider`: `local` (on-device Whisper, `stt.local.model` default `base` from `tiny`, `base`, `small`, `medium`, `large-v3`, `turbo`; voice-activity filtering on by default with `vad_min_silence_ms` 500, and a segment dropped only when its no-speech probability exceeds 0.6 and its average log-probability is below -1.0; `unload_after_idle_secs` default 0 = never), `whisper-cpp`, `groq` (`whisper-large-v3-turbo`), `openai` (`whisper-1`; `timeout_secs` 60), `mistral` (`voxtral-mini-latest`), `xai` (`grok-voice-transcribe-2.0`), `elevenlabs` (`scribe_v2`, optional `diarize`, `tag_audio_events`), `deepinfra` and `nous`, with global `stt.language` (default `en`; empty = auto-detect) overridden by `stt.<provider>.language`; before cloud upload the system SHALL trim silence when `stt.cloud_trim_silence` is true (default; threshold `stt.cloud_trim_threshold_db` -40, keeping `stt.cloud_trim_keep_ms` 300 of each pause; on trim failure the raw audio is uploaded) and SHALL apply each provider's declared upload limits (openai and groq 25 MiB, mistral 500 MiB and 3,600 s, xai 500 MiB, elevenlabs under 5 GiB) per `voice-mode` › Long recordings and provider upload limits. When `stt.provider` is set, that provider SHALL be used strictly and a failure SHALL return `stt is configured to use <provider>, but <reason>`; when it is unset, the system SHALL use the first available of `local`, `whisper-cpp`, `groq`, `openai`, `mistral`, `xai`, `elevenlabs`, `deepinfra` and record the choice in the transcript metadata; with none available the caller SHALL receive `no STT provider available`.
**Milestone:** M2

#### Scenario: Explicit provider is strict
- **GIVEN** `stt.provider = "groq"` and no Groq key
- **WHEN** a voice note is transcribed
- **THEN** the system SHALL return `stt is configured to use groq, but GROQ_API_KEY is not set` and SHALL NOT try another provider

#### Scenario: Unset provider picks local first
- **GIVEN** no `stt.provider` and the local Whisper runtime installed
- **WHEN** audio is transcribed
- **THEN** the system SHALL use `local` with model `base`

### Requirement: Command-template speech providers
The system SHALL accept user-declared providers under `tts.providers.<name>` and `stt.providers.<name>` with `type = "command"` and a `command` template, selectable by `tts.provider` / `stt.provider`, where built-in provider names always take precedence over a same-named declaration; TTS templates SHALL support `{input_path}` (alias `{text_path}`), `{output_path}`, `{format}`, `{voice}` and `{model}` with keys `timeout_secs` (default 120, an idle deadline reset by any output), `output_format` (`mp3` default, `wav`, `ogg`, `flac`, `m4a`, `aac`, `amr`, `opus`), `voice_compatible` (default false), `max_text_length` (default 5,000) and optional `warm_command` / `release_command`; STT templates SHALL support `{input_path}`, `{output_path}`, `{output_dir}`, `{format}` (`txt` default, `json`, `srt`, `vtt`), `{language}` and `{model}` with `timeout_secs` (default 300) and `normalize` (default false; transcode to 16 kHz mono first), reading the transcript from `{output_path}` when non-empty, else stdout, else failing with `command STT provider wrote no output file and produced no stdout`; placeholder values SHALL be shell-quoted for their quoting context, `{{`/`}}` SHALL produce literal braces, commands SHALL run inside the execution sandbox under the profile's `process.spawn` grant with the whole process tree killed on timeout, and a non-zero exit, empty output or timeout SHALL return an error including the command's stderr tail.
**Milestone:** M2

#### Scenario: Paths with spaces
- **GIVEN** a TTS command provider `command = "mytts --in {input_path} --out {output_path}"`
- **WHEN** the output path contains a space
- **THEN** the rendered command SHALL quote the path so the engine receives it as one argument

#### Scenario: Built-in name wins
- **GIVEN** `stt.providers.openai = { type = "command", command = "x {input_path}" }` and `stt.provider = "openai"`
- **WHEN** audio is transcribed
- **THEN** the built-in `openai` provider SHALL be used and the command SHALL NOT run

### Requirement: Media tool credentials and billing are explicit
Every web and media tool result SHALL record the provider id and `billing_source` (grammar of `observability` › Per-call usage ledger: `subscription:<provider>[/<label>]`, `api_key:<provider>[/<label>]`, `keyless:<vendor>` or `local`) in its metadata and in the usage ledger, and the system SHALL show the resolved provider, billing source and availability reason for every web and media category in `prometheus tools explain <tool>` and `prometheus doctor`.
**Milestone:** M2

#### Scenario: Subscription-billed image
- **GIVEN** `image_gen.provider = "nous"` with a valid Nous sign-in
- **WHEN** an image is generated
- **THEN** its result metadata and ledger row SHALL carry `billing_source = "subscription:nous"`
