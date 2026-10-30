# Hermes → Prometheus feature parity

Every user-facing Hermes Agent feature (inventoried from Hermes `main` at 088f657ecf: docs, CLI and slash-command registries, toolsets, platforms, providers, plugins and config sections) mapped to the Prometheus requirement that specifies it. Requirements live in `openspec/changes/bootstrap-prometheus-agent/specs/<capability>/spec.md`.

Status: **Specified** (a requirement covers it), **N/A** (intentionally not carried over, with the reason).

Totals: Specified: 995, N/A: 8.

## Tools and toolsets

| ID | Hermes feature | Status | Prometheus requirement |
|---|---|---|---|
| TOOL-001 | Central tool registry (name, toolset, schema, handler, `check_fn`, emoji, `max_result_size_chars`) | Specified | tool-system › Typed tool registry; tool-system › Availability probes are cached per profile with flake grace; tool-system › Result budget and spillover (per-tool declared cap) |
| TOOL-002 | Per-model dynamic schema overrides (`dynamic_schema_overrides`: `patch` gains V4A `mode`/`patch` params only for OpenAI-family mains; `read_file` description upgrades "PDF (text layer)" to "scanned or text" when hosted OCR is reachable) | Specified | tool-system › Model-dependent schema variants |
| TOOL-003 | Core tool list `_HERMES_CORE_TOOLS` (web_search, web_extract, terminal, process_manage, read_file, write_file, patch, search_files, vision_analyze, image_generate, skills_list, skill_view, skill_manage, browser_exec, browser_cdp, browser_dialog, browser_vault, text_to_speech, todo_list, memory, session_search, clarify, execute_code, delegate_task, cronjob_manage, 13 kanban_* tools, computer_use, manage_connections) | Specified | tool-system › Core toolset membership |
| TOOL-004 | Static single-purpose toolsets: `web`, `search` (web_search only), `x_search`, `vision`, `video` (video_analyze, opt-in), `image_gen`, `video_gen`, `computer_use`, `terminal`, `skills`, `browser`, `cronjob`, `file`, `tts`, `todo`, `memory`, `context_engine`, `session_search`, `connections`, `clarify`, `code_execution`, `delegation`, `kanban` | Specified | tool-system › Single-purpose toolsets and default-off toolsets |
| TOOL-005 | Composite `debugging` toolset (terminal + process + includes web, file) and `safe` toolset (web + vision + image_gen, no terminal) | Specified | tool-system › Composite toolsets and the coding posture |
| TOOL-006 | `coding` composite (core minus image_generate, text_to_speech, cronjob_manage, computer_use; keeps kanban; `posture=True` flag) | Specified | tool-system › Composite toolsets and the coding posture (existing "Built-in toolsets and composition" tightened) |
| TOOL-007 | `hermes-acp` toolset (coding tools minus clarify) | Specified | acp-adapter › Clarify is part of the ACP toolset (Prometheus deliberately keeps clarify; documented divergence) |
| TOOL-008 | `hermes-api-server` toolset (core minus text_to_speech, clarify, computer_use; no interactive tools) | Specified | api-server › Unattended approvals and the API toolset |
| TOOL-009 | `hermes-cli` and `hermes-cron` bundles (full core for CLI; cron = same core, gated by `hermes tools`) | Specified | tool-system › Per-surface default toolsets |
| TOOL-010 | Per-platform messaging bundles `hermes-<platform>` (telegram, discord + discord/discord_admin, whatsapp, slack, signal, bluebubbles, email, mattermost, matrix, dingtalk, feishu + feishu tools, weixin, qqbot, wecom, wecom-callback, yuanbao + yb tools, sms) and union `hermes-gateway`; implicit bundle for plugin platforms | Specified | tool-system › Platform bundles |
| TOOL-011 | Disabling a bundle never strips core tools (`bundle_non_core_tools`) | Specified | tool-system › Disabling a bundle never strips core |
| TOOL-012 | `hermes-webhook` safe set (web_search, web_extract, vision_analyze, clarify only) | Specified | messaging-gateway › Webhook route delivery and safe default toolset |
| TOOL-013 | `all` / `*` expands to every toolset except session-platform-gated ones | Specified | tool-system › Built-in toolsets and composition |
| TOOL-014 | Plugin/MCP registry toolsets and aliases (MCP server name usable as a toolset alias in enabled_toolsets) | Specified | tool-system › Registry toolset aliases |
| TOOL-015 | Toolset distributions for batch data generation (`default`, `image_gen`, `research`, `science`, `development`, `safe`, `balanced`, `minimal`, `terminal_only`, `terminal_web`, `creative`, `reasoning`, `browser_use`, `browser_only`, `browser_tasks`, `terminal_tasks`, `mixed_tasks`; independent % rolls, `a+b` compound entries roll once, fallback to highest-probability entry when nothing rolled) | Specified | replay-and-evaluation › Toolset distributions |
| TOOL-016 | Tool search bridge (`tool_search`, `tool_describe`, `tool_call`; `tools.tool_search.enabled: auto`, `threshold_pct` 5% of context, `listing` auto with `listing_max_tokens` 4000, `search_default_limit` 5, `max_search_limit` 25, BM25 ranking, default `defer` list incl. computer_use, session_search, image_generate, todo_list, process_manage, cronjob_manage, browser_vault and desktop UI tools) | Specified | tool-system › Tool search listing, ranking and limits (existing "Tool search progressive disclosure" tightened: threshold_pct) |
| TOOL-017 | Connectors via tool search (remote gateway tools named `connectors__<app>__<tool>`, gateway search bounded at 30 s degrading to local results, `tool_describe`/`tool_call` proxied to gateway, slug retry) | Specified | connectors › Remote connector tools through the search bridge; › Connector calls run through the dispatch pipeline |
| TOOL-018 | `manage_connections` tool (actions display, status, connect, reconnect, rename for connector accounts; install, enable, authorize for MCP targets; `connectors`, `force`; alias pattern `^[a-z0-9][a-z0-9-]{0,31}$`) | Specified | connectors › manage_connections tool; › Connection flows require the user |
| TOOL-019 | `manage_catalog` tool (desktop only; actions search, install; item kinds plugin/skill; `reason`) routed through the approval card | Specified | plugin-system › Catalog tool for agent-initiated installs |
| TOOL-020 | `read_file` (offset default 1, limit 2000 max 2000, ~100K char budget + next_offset, LINE_NUM\|CONTENT format, binary/device refusal, dedup) | Specified | execution-environments › read_file guards, pagination and dedup |
| TOOL-021 | Document extraction in `read_file` (.ipynb, .docx, .xlsx, SQLite schema overview built in; PDF/legacy Office/ODF/RTF/EPUB via lazily installed converter; Markdown output paginated; scanned-PDF coverage warning when >20% or ≥10 pages lack text; notebook outputs truncated at 20,000 chars; extraction works across remote backends; optional hosted OCR) | Specified | execution-environments › read_file document extraction |
| TOOL-022 | `read_file` niceties: UTF-16 detection and transcoding; merge-conflict blocks flagged (`conflict_blocks: N`); similar-filename suggestions on not-found | Specified | execution-environments › read_file encoding, conflict markers and path suggestions |
| TOOL-023 | `write_file` (path, content; guards, stale-overwrite protection) | Specified | execution-environments › Write guards; execution-environments › Stale overwrite protection |
| TOOL-024 | `patch` (old_string/new_string, `replace_all`, uniqueness requirement, 9 fuzzy-match strategies (exact, line-trimmed, whitespace-normalized, indentation-flexible, unicode-normalized…), returns unified diff, V4A multi-file patch mode) | Specified | execution-environments › patch tool contract |
| TOOL-025 | `search_files` (pattern; `target` content\|files; path; file_glob; limit; offset; `order` discovery\|modified; `output_mode` content\|files_only\|count; `context` lines; ripgrep-backed) | Specified | execution-environments › search_files contract |
| TOOL-026 | Post-write lint after `write_file`/`patch` (in-process syntax check, then LSP diagnostics; only errors introduced by the edit are reported; failures fall back silently to syntax-only) | Specified | execution-environments › Post-write diagnostics |
| TOOL-027 | LSP subsystem (pyright, gopls, rust-analyzer, tsserver, clangd and ~20 more; only inside git workspaces; workspace trust (`lsp.trusted_workspaces`) before servers that execute project code; `wait_mode` document, `wait_timeout` 5 s, `idle_timeout` 600 s, `install_strategy` auto via `package_manager` npm, `exclude_roots`, custom `servers`) | Specified | lsp-diagnostics › Workspace trust (+ Workspace gating; Built-in server registry; Diagnostics introduced by an edit; Wait modes and timeouts; Server installation; Server overrides and custom servers) |
| TOOL-028 | `hermes lsp` CLI (`status`, `list [--installed-only]`, `install <id>`, `install-all`, `restart`, `which <id>`) | Specified | lsp-diagnostics › LSP CLI |
| TOOL-029 | `terminal` tool (command, background, timeout, workdir, pty, notify, heartbeat, persist_on_release; default timeout 180) | Specified | execution-environments › Terminal argument validation; Foreground cap, promotion and server guard; Foreground result shaping; Terminal configuration defaults |
| TOOL-030 | Trailing `&` auto-backgrounding with `auto_backgrounded` note; refusal of mixed shapes (`a; b &`, nohup/disown/setsid, command substitution) | Specified | execution-environments › Foreground cap, promotion and server guard |
| TOOL-031 | `process_manage` tool (list, poll, log, wait, kill, write, submit, close, handoff; offset/limit on log) | Specified | execution-environments › Background process registry; Subagent process handoff (Prometheus name `process`) |
| TOOL-032 | Repeated quiet waits auto-switch to completion notification (`terminal.process_wait_auto_notify` true: 2 unchanged wait/poll calls turn on notify_on_complete and tell the model to stop polling; not in one-shot, cron, kanban, API, subagents) | Specified | execution-environments › Quiet-wait auto-notify |
| TOOL-033 | Retained completed-process receipts (newest 64 completed results kept up to 7 days, readable only from the owning conversation or its compressed continuation; `list` includes retained results) | Specified | execution-environments › Retained process receipts |
| TOOL-034 | Sudo support (masked password prompt cached per session for direct `sudo`/`env … sudo`; `SUDO_PASSWORD` secret for headless; subagents cannot prompt; messaging hint on failure) | Specified | execution-environments › Sudo password handling |
| TOOL-035 | Shell startup handling (`terminal.auto_source_bashrc` true, `terminal.shell_init_files`, `persistent_shell`, non-interactive guidance) | Specified | execution-environments › Shell startup files |
| TOOL-036 | Non-secret env passthrough (`terminal.env_passthrough`, `client_env_passthrough`, `docker_forward_env`, `docker_env`) | Specified | execution-environments › Non-secret environment passthrough |
| TOOL-037 | Local backend | Specified | execution-environments › Local backend runs under the OS sandbox |
| TOOL-038 | Docker backend options (`docker_image`, `docker_volumes`, `docker_mount_cwd_to_workspace`, `docker_network: false` → `--network=none`, `docker_extra_args`, `docker_run_as_host_user`, `docker_shared_container_key`, `docker_snap_compat`, persistent container) | Specified | execution-environments › Container backend options |
| TOOL-039 | SSH backend (persistent shell, file sync to/from remote, `sync_back_max_bytes` 2 GiB) | Specified | execution-environments › Remote file sync and sync-back |
| TOOL-040 | Singularity/Apptainer backend (official plugin; `terminal.singularity_image` SIF) | Specified | execution-environments › First-party backend plugins |
| TOOL-041 | Modal backend (direct with Modal creds, or managed via Nous gateway; `terminal.modal_mode` auto) | Specified | execution-environments › First-party backend plugins |
| TOOL-042 | Daytona backend | Specified | execution-environments › Remote sandboxes are plugins |
| TOOL-043 | Vercel Sandbox backend (`vercel_image`, token/project/team or OIDC token) | Specified | execution-environments › Remote sandboxes are plugins |
| TOOL-044 | Container resources (cpu 1, memory 5120 MB, disk 51200 MB, shm 1g, persistent) | Specified | execution-environments › Terminal configuration defaults |
| TOOL-045 | Container security hardening | Specified | execution-environments › Container hardening is never silently dropped |
| TOOL-046 | Degraded backend results (`terminal.degraded_mode`) | Specified | execution-environments › Infrastructure failures are degraded results |
| TOOL-047 | Signal-death explanations in results (exit -9/137 → "terminated by signal 9: SIGKILL — often OOM") | Specified | execution-environments › Foreground result shaping (`exit_code_meaning`) |
| TOOL-048 | `execute_code` tool (code; RPC tool stubs; 50 tool calls; 300 s; stdout 50 KB head/tail with full output saved; stderr 10 KB; env scrubbing) | Specified | subagent-delegation › Programmatic tool calling under the caller's grant; execute_code RPC channel and limits; execute_code pre-execution guards |
| TOOL-049 | Persistent per-session Python kernel (`reset` arg; state persists between calls; dies on timeout/interrupt; `kernel_idle_timeout` 1800 s; `max_session_kernels` 4 with oldest evicted; remote-backend kernels with fallback to one-shot scripts; tool authority re-bound each cell) | Specified | subagent-delegation › Persistent execute_code session kernel |
| TOOL-050 | `execute_code` execution modes (`code_execution.mode`: `project` default = session cwd + active venv/conda python with fallback; `strict` = temp staging dir + bundled interpreter) | Specified | subagent-delegation › execute_code execution modes |
| TOOL-051 | `browser_exec` (model-written Python against browser-harness helpers: new_tab, goto_url, wait_for_load, page_info, js, fill_input, click_at_xy, capture_screenshot, cdp, ensure_real_tab; `code`, `timeout_s` default 300 max 1800, named `session` = own driver+browser; per-session workspace dir; screenshots attached for vision models; only offered when the session also has `terminal`) | Specified | execution-environments › browser_exec tool |
| TOOL-052 | `browser_cdp` (raw CDP method + params, target_id, frame_id, timeout; only while a CDP endpoint is configured) | Specified | execution-environments › browser_cdp and browser_dialog tools |
| TOOL-053 | `browser_dialog` (accept\|dismiss, prompt_text, dialog_id) | Specified | execution-environments › browser_cdp and browser_dialog tools |
| TOOL-054 | `browser_vault` + credential vault (actions list, fill, save_login, enter_code (2FA/TOTP), unlock; backends local encrypted vault, 1Password `op`, Bitwarden `bw`; masked save-login prompt/card; secrets injected into page, model only sees `{filled_fields, origin}`; origin binding; `vault.key_storage` file\|keychain; headless behaviour; payment cards and addresses) | Specified | security-boundaries › browser_vault fills by handle (+ Credential vault storage; Vault codes, cards and addresses; Password manager login sources) |
| TOOL-055 | Local Chromium resolution and lifecycle (executable_path → managed Chromium → image path → system chromium; one browser per session, throwaway profile, headless, 120 s inactivity close, orphan sweep; `chromium_args`; `command_timeout` 30) | Specified | execution-environments › Browser session resolution; Local browser isolation; Browser supervision, dialogs and cleanup (missing only `chromium_args`/`command_timeout`, minor) |
| TOOL-056 | Headed mode (`browser.headed: true` visible window) | Specified | execution-environments › Local browser options and headed mode |
| TOOL-057 | Cloud browser providers (Nous subscription, Browserbase, Browser Use cloud, Firecrawl, plugin providers; auto-select from present API keys on never-configured setups) | Specified | execution-environments › First-party cloud browser providers |
| TOOL-058 | Hybrid routing: private/LAN URLs go to a local browser even when a cloud provider is active (`browser.auto_local_for_private_urls` true) | Specified | execution-environments › Private URLs route to a local browser |
| TOOL-059 | Real-profile browsing (managed copy of the user's everyday browser profile; `use_real_profile`, `real_profile_autoclose`, `real_profile_pin`; `local` flag on browser_exec; scheduled/unattended runs) | Specified | execution-environments › Real-profile browsing |
| TOOL-060 | Lightpanda local engine (`browser.engine` auto\|chromium\|lightpanda; one process per session; shared HTTP cache; no screenshots; shadowed by cloud/cdp/real-profile settings, reported by status/doctor) | Specified | execution-environments › Lightpanda engine |
| TOOL-061 | `/browser connect [ws-url]` (auto-launch local Chromium-family on 9222), `/browser status`, `/browser disconnect` (CLI-only) | Specified | execution-environments › Connecting to the user's browser |
| TOOL-062 | Camofox (anti-detection Firefox via REST, `browser.camofox` config) | Specified | plugin-system › Catalog plugins for Home Assistant and Camofox |
| TOOL-063 | Turning browsing off / browser gating per platform | Specified | tool-system › Toolset selection is validated (disable `browser`) |
| TOOL-064 | Browser URL safety (secret-in-URL, metadata, private hosts) | Specified | execution-environments › Browser URL safety |
| TOOL-065 | Website blocklist (`security.website_blocklist`: enabled, domains with globs, shared list files; applies to web/browser URL tools) | Specified | security-boundaries › Website blocklist |
| TOOL-066 | Bot Desktop screen (browser on a visible bot desktop) | Specified | execution-environments › Bot screen |
| TOOL-067 | `vision_analyze` (image_url (URL or local path), question, `region` crop; auxiliary vision model) | Specified | web-and-media-tools › vision_analyze tool |
| TOOL-068 | `video_analyze` (video_url, question; opt-in `video` toolset) | Specified | web-and-media-tools › video_analyze tool |
| TOOL-069 | Image input routing (`agent.image_input_mode` auto\|native\|text: native pixels for vision models, auxiliary description for text-only models; SVG/non-raster handling on Responses backends) | Specified | web-and-media-tools › Image input routing |
| TOOL-070 | Image budget settings (`vision.embed_target_bytes` 256 KiB recompression, `vision.max_calls_per_image`, `vision.replay_recent_images` 3 on turns rebuilt from storage) | Specified | web-and-media-tools › Image embed budget, per-image cap and replay |
| TOOL-071 | Clipboard image attach in CLI (`/paste`, Ctrl/Cmd+V, `/terminal-setup` for VS Code/Cursor/Windsurf, badges `[📎 Image #n]`, saved to `images/`, platform clipboard backends incl. WSL2/Wayland, SSH workarounds) | Specified | tui › Clipboard image attachments |
| TOOL-072 | `image_generate` tool (prompt, aspect_ratio landscape\|square\|portrait; image-to-image/editing via input image on supporting backends; per-call opt-in `upscale`; result delivered as media) | Specified | web-and-media-tools › image_generate tool |
| TOOL-073 | Image-gen first-party providers/models (FAL default `fal-ai/flux-2/klein/9b` + 10 models, OpenAI GPT Image (quality), OpenAI-compatible custom endpoint, OpenRouter catalog, xAI, Krea, Meta Muse, DeepInfra, Codex, Nous subscription row) | Specified | web-and-media-tools › Built-in image generation providers and models |
| TOOL-074 | `video_generate` tool (prompt, duration, aspect_ratio, resolution, optional image_url for image-to-video, reference-to-video; providers fal, xai, openrouter, deepinfra) | Specified | web-and-media-tools › video_generate tool |
| TOOL-075 | `xai_video_edit` / `xai_video_extend` (prompt, video_url; only with xAI video creds) | Specified | web-and-media-tools › Provider-contributed media tools |
| TOOL-076 | `text_to_speech` tool (text, output_path, speed, instructions; input length limits; Telegram voice bubble via ffmpeg Opus conversion) | Specified | web-and-media-tools › text_to_speech tool and delivery |
| TOOL-077 | TTS providers and defaults (edge default voice en-US-AriaNeural, elevenlabs (agent-selected voices), openai gpt-4o-mini-tts/alloy, gemini (persona prompts, audio tags), xai (custom voices/cloning, speed 0.7–1.5), mistral, minimax, kittentts, neutts, piper (local, 44 langs), deepinfra; `keep_warm_seconds` 60 for local engines) | Specified | web-and-media-tools › Built-in text-to-speech providers |
| TOOL-078 | TTS/STT custom command providers (shell command templates as providers) | Specified | web-and-media-tools › Command-template speech providers |
| TOOL-079 | STT providers and config (`stt.enabled`, `echo_transcripts`, local faster-whisper `base` default with VAD, groq, openai whisper-1, mistral voxtral, xai, elevenlabs scribe_v2, deepinfra, whisper.cpp; cloud silence trimming; fallback chain) | Specified | web-and-media-tools › Built-in speech-to-text providers and resolution |
| TOOL-080 | CLI/TUI voice mode (`/voice`, push-to-talk `voice.record_key` ctrl+b, `max_recording_seconds` 120, silence detection threshold 200 / 3.0 s, beeps, thinking sound, barge-in with grace 0.5 s, stop phrases, `submit_mode` direct\|draft, streaming STT/TTS, auto_tts, Whisper hallucination filter) | Specified | voice-mode › Voice mode toggle and commands; voice-mode › Push-to-talk recording and continuous loop; voice-mode › Two-stage silence detection; voice-mode › Spoken and typed stop phrases; voice-mode › Barge-in; voice-mode › Live streaming transcription; voice-mode › Sentence-streamed spoken replies; voice-mode › Transcription hallucination filter |
| TOOL-081 | Desktop voice: client-direct voice path and GPT-Live full-duplex voice chat mode (`voice.voice_chat_mode` chained\|gpt_live, delegates actions to the agent) | Specified | voice-mode › Client-direct voice for remote daemons; voice-mode › Realtime full-duplex voice chat mode (upgrade); voice-mode › Desktop voice controls |
| TOOL-082 | Gateway voice reply commands `/voice on\|off\|tts` (modes off, voice_only, all) | Specified | voice-mode › Gateway per-chat voice reply command |
| TOOL-083 | Discord voice channels (bot joins VC, listens, transcribes, speaks replies; echo prevention; access control; text-channel integration) | Specified | voice-mode › Discord voice channel sessions; › Voice channel echo suppression, access control and rebinding |
| TOOL-084 | `web_search` tool (query, limit) | Specified | web-and-media-tools › web_search tool contract |
| TOOL-085 | `web_extract` tool (urls list, char_limit; default 15,000 clamped 2,000–500,000; ~75% head / 25% tail on line boundaries + `[TRUNCATED]` footer with stored full-text path; stored text capped at 2 MB; `extract_timeout` 120 s per dispatch; search-only backend error) | Specified | web-and-media-tools › web_extract tool contract; › Extracted full text is stored and capped |
| TOOL-086 | Web backends (firecrawl default, searxng, brave_free, ddgs, exa, parallel, tavily, perplexity, keenable, xai, openai_native; search-only vs search+extract) | Specified | web-and-media-tools › Built-in web backends with capability flags |
| TOOL-087 | Keyless free-tier rotation (no credentials → round-robin across Exa, Parallel, Firecrawl, Keenable public tiers; `web.keyless_fallback`, `keyless_rescue` on keyed-backend failure; Free vs Paid rows) | Specified | web-and-media-tools › Keyless web search rotation is explicit opt-in |
| TOOL-088 | Web result caching (search memo per process keyed by normalized query+provider; extract full text cached under cache/web shared across processes; TTL 20 min; concurrent identical searches coalesced; only successes cached; localhost/private hosts never cached; `cache_exempt_hosts`) | Specified | web-and-media-tools › Web result caching |
| TOOL-089 | `x_search` tool (query, allowed/excluded handles, from/to date validation, image/video understanding; xAI Responses built-in tool; model grok-4.5, timeout 180 s, retries 2; `XAI_API_KEY` preferred over SuperGrok OAuth; `degraded: true` when no citations; off by default) | Specified | web-and-media-tools › x_search tool |
| TOOL-090 | `todo_list` tool (todos list of items with id/content/status, `merge` flag) | Specified | agent-turn-loop › todo tool contract |
| TOOL-091 | `clarify` tool (batched questions, multiple-choice or open-ended; batch UI; gateway rendering) | Specified | agent-turn-loop › clarify tool contract |
| TOOL-092 | `memory` tool | Specified | memory › Memory tool surface |
| TOOL-093 | `session_search` tool (query, limit, sort newest\|oldest, detail adaptive\|full, after/before, exclude_session_ids, session_id + around_message_id + window, role_filter, profile; summarization) | Specified | session-storage › Session search tool |
| TOOL-094 | `skills_list` / `skill_view` | Specified | skills › skills_list and skill_view tools |
| TOOL-095 | `skill_manage` | Specified | skills › skill_manage tool validation |
| TOOL-096 | `delegate_task` | Specified | subagent-delegation › delegate_task tool contract |
| TOOL-097 | `cronjob_manage` tool (fields incl. pinned, paused_reason, monitor, no_agent, script, context_from, continuity, enabled_toolsets, workdir, timezone, attach_to_session, failure_deliver, skills, repeat) | Specified | scheduled-jobs › Complete cronjob tool field set (+ Monitor mode; Failure notice routing; Continuable deliveries; Job chaining and continuity; Model pinning and cron fleet default) |
| TOOL-098 | Kanban worker tools (kanban_show, list, complete, block, request_review, request_changes, heartbeat, comment, create, link, unblock, attach) | Specified | kanban › Kanban toolset and grants |
| TOOL-099 | `kanban_attach_url` and `kanban_attachments` | Specified | kanban › URL attachments and attachment listing tools |
| TOOL-100 | `computer_use` tool (actions capture, click, double_click, right_click, middle_click, drag, scroll, type, key, set_value, wait, list_apps, list_windows, focus_app; capture modes som\|vision\|ax; background input without cursor/focus steal via cua-driver MCP; any tool-capable model; `max_image_dimension` 1456; `ax_max_elements` 200; capture is free, other actions need approval) | Specified | computer-use › computer_use tool actions (+ Capture modes and payload budget; Approval of non-capture actions) |
| TOOL-101 | Computer-use permission modes (`standard`, `bounded` with reviewed capability manifest, `unrestricted` under yolo), `hermes computer-use install\|status\|doctor\|install-local`, autostart, telemetry opt-in, unsigned driver gate | Specified | computer-use › Permission modes are fixed per runtime (+ Driver runtime management and identity; Computer-use CLI and doctor) |
| TOOL-102 | `message_agent` tool (bot mode: send a message to another agent; target, message; authorization check) | Specified | bot-mode › message_agent tool; bot-mode › Mentions from any chat |
| TOOL-103 | `react_to_message` (emoji, message_row_id, messages_back) | Specified | desktop-app › Desktop UI toolset contract; desktop-app › Agent reactions |
| TOOL-104 | `read_terminal` (start_line, count) — read the in-app terminal pane | Specified | desktop-app › Terminal pane tools |
| TOOL-105 | `close_terminal` (process_id) — close an in-app terminal pane | Specified | desktop-app › Terminal pane tools |
| TOOL-106 | `desktop_preview` (open\|close\|read, url, label, start, count) — in-app browser/preview pane | Specified | desktop-app › Preview pane tool |
| TOOL-107 | `drive_preview` (action, ref, selector, text, submit, key, amount, to, max, full) — drive the in-app preview pane | Specified | desktop-app › Driving the preview page |
| TOOL-108 | `annotate_preview` (action, ref, selector, label) | Specified | desktop-app › Annotating the preview page |
| TOOL-109 | `read_window_below` — read the window behind the app | Specified | desktop-app › Reading the window below |
| TOOL-110 | `focus_pane` (pane) | Specified | desktop-app › Pane focus, tours and tips |
| TOOL-111 | `gui_tour` (actions, surface, steps, presets) and `show_tip` (text, selector, title, side) | Specified | desktop-app › Pane focus, tours and tips |
| TOOL-112 | `set_theme` (accent, mode, layout, picker) | Specified | desktop-app › Appearance tool |
| TOOL-113 | `desktop_project` (create\|switch\|list, name, path) — `project` toolset for GUI/TUI sessions | Specified | desktop-app › Project tool |
| TOOL-114 | `setup_beat` (onboarding cards: beat, question, value, skip, suggest, options; `onboarding` toolset only in the onboarding chat) | Specified | desktop-app › First-run onboarding chat |
| TOOL-115 | `discord` tool (fetch_messages, search_members, create_thread) and `discord_admin` (list channels/roles, pins, member_info, assign roles…); `discord.server_actions` allowlist; intent-gated actions hidden/annotated | Specified | platform-adapters › Discord tools; › Platform-contributed toolsets |
| TOOL-116 | Feishu/Lark tools: `feishu_doc_read`, `feishu_drive_list_comments`, `feishu_drive_list_comment_replies`, `feishu_drive_reply_comment`, `feishu_drive_add_comment` | Specified | platform-adapters › Feishu and Yuanbao tools; › Platform-contributed toolsets |
| TOOL-117 | Yuanbao tools: `yb_query_group_info`, `yb_query_group_members`, `yb_send_dm`, `yb_search_sticker`, `yb_send_sticker` | Specified | platform-adapters › Feishu and Yuanbao tools; › Platform-contributed toolsets |
| TOOL-118 | Google Meet plugin tools (`meet_join`, `meet_status`, `meet_transcript`, `meet_say`, `meet_leave`, `meet_requirements`) + `meet` command | Specified | plugin-system › First-party google_meet plugin |
| TOOL-119 | A2A plugin tools (`a2a_discover`, `a2a_call`, `a2a_list`, `a2a_history`, `a2a_orchestrate` modes all\|first…; `a2a_agents` config) | Specified | agent-interop › Outbound A2A tools |
| TOOL-120 | MCP client (stdio + HTTP/SSE servers in `mcp_servers`) | Specified | plugin-system › MCP client servers |
| TOOL-121 | MCP naming, include/exclude filtering incl. globs, disable server, filtering of utility tools | Specified | tool-system › MCP tool naming and filtering |
| TOOL-122 | MCP runtime (lazy start, discovery timeouts 1.5 s / 15 s single-query, discovery concurrency 4, stdio env filtering) | Specified | tool-system › MCP runtime safety |
| TOOL-123 | MCP OAuth 2.1 for HTTP servers (`auth: oauth`; CIMD client identification with DCR fallback; PKCE; `oauth.client_name` override; token storage and refresh; parked server when refresh dies, never opening a browser in background; dashboard/device flows) | Specified | plugin-system › MCP OAuth for HTTP servers |
| TOOL-124 | MCP mTLS (`client_cert` combined PEM, cert+key pair, `client_key`) | Specified | plugin-system › MCP mTLS and identity header |
| TOOL-125 | MCP per-user identity header (`identity_header.name`, `value_from` static\|profile, `value`) | Specified | plugin-system › MCP mTLS and identity header |
| TOOL-126 | MCP utility tools `list_resources`, `read_resource`, `list_prompts`, `get_prompt` per server (registered only when the server advertises the capability) | Specified | tool-system › MCP resource and prompt utility tools |
| TOOL-127 | MCP dynamic tool discovery (`notifications/tools/list_changed` → lock-protected refresh) | Specified | tool-system › MCP tool list changes respect the frozen surface |
| TOOL-128 | MCP reload (`/reload-mcp`; gateway auto-reload on config change within ~1 min, `mcp.auto_reload_on_config_change` true; reload re-probes availability-gated tools) | Specified | plugin-system › MCP config watch, recycling and discovery wait |
| TOOL-129 | MCP stdio server recycling (idle/lifetime limits; tools stay registered; transparent restart on next call) | Specified | plugin-system › MCP config watch, recycling and discovery wait |
| TOOL-130 | `hermes mcp` CLI (add with `--preset codex`, list, remove, test, configure tool selection) | Specified | plugin-system › mcp CLI |
| TOOL-131 | MCP catalog one-click install (Nous-reviewed entries disabled by default; tool selection at install; trust model; manifest version compatibility; `${ENV_VAR}` runtime substitution; entries needing own OAuth app; `suggest:` metadata; setup card via manage_connections) | Specified | plugin-system › MCP catalog entries |
| TOOL-132 | MCP result sanitization (strip Unicode TAG chars U+E0000–E007F; surface vendor `_meta`, hide protocol-reserved keys) | Specified | tool-system › MCP result sanitization |
| TOOL-133 | MCP image results saved to image cache and reported as `MEDIA:<path>` | Specified | tool-system › MCP media results |
| TOOL-134 | MCP sampling (`sampling/createMessage`, enabled by default per server; model override; limits) | Specified | plugin-system › MCP sampling |
| TOOL-135 | MCP elicitation (form-mode routed through approval surface; enabled by default; timeout 300 s) | Specified | plugin-system › MCP elicitation |
| TOOL-136 | MCP tool costs and spend limits (`tool_costs` per tool, credits ranges, per-chat budget) | Specified | plugin-system › MCP tool costs and spend limits |
| TOOL-137 | MCP events (experimental webhook delivery; events start agent runs via webhook listener; public HTTPS callback URL) | Specified | messaging-gateway › Webhook route delivery and safe default toolset |
| TOOL-138 | Hermes as an MCP server | Specified | plugin-system › MCP server mode |
| TOOL-139 | Context references (`@file:path[:a-b]`, `@folder:`, `@diff`, `@staged`, `@git:N` max 10, `@url:`; appended under `--- Attached Context ---`; soft limit 25% / hard limit 50% of context; folder 200 entries; sensitive path blocklist (~/.ssh, ~/.aws, ~/.gnupg, ~/.kube, rc files, .netrc, .env); traversal protection; binary detection; CLI tab completion) | Specified | agent-turn-loop › Context references in user input; Context reference safety and completion |
| TOOL-140 | Deliverable mode (gateway extracts bare absolute or `~/` file paths with supported extensions from replies, strips them from text, uploads natively by type; ignores code blocks; kanban completion artifacts) | Specified | platform-adapters › Deliverable mode |
| TOOL-141 | Checkpoints (auto before write_file/patch and destructive terminal commands; ≤1 per directory per turn; shared shadow git store with per-project refs; `checkpoints.enabled` false, max_snapshots 20, max_total_size_mb 500, max_file_size_mb 10, retention 7 days; nested-repo and container-backend guards) | Specified | execution-environments › Snapshot triggers, caps and per-run toggle |
| TOOL-142 | `/rollback` (list with change stats, `<N>` keeps hand-edits and undoes last chat turn, `--all`, `diff <N>`, `<N> <file>` single-file restore) | Specified | execution-environments › Rollback listing, diff and selective restore |
| TOOL-143 | `hermes checkpoints` CLI (status/list, prune, clear (confirm), clear-legacy) | Specified | execution-environments › Snapshot store management CLI |
| TOOL-144 | `/worktree new [name]` / `/worktree` / `/worktree list` (creates `.worktrees/<name>` on branch `hermes/<name>` from fetched remote tip, retargets session tools without restart) | Specified | git-worktrees › In-session worktree command |
| TOOL-145 | `hermes -w` automatic worktree mode (temp worktree + branch, repo hooks/fsmonitor/filters disabled; exit prompt keep/archive/remove for dirty or unpushed trees; one-shot/cron/gateway never prompt) | Specified | git-worktrees › Exit disposition (+ chat -w worktree mode; Worktree creation) |
| TOOL-146 | Stale worktree sweep and restore (repos recorded in `worktree_repos.json`; sweep at `-w` start and every 6 h; tiers 24 h / 72 h; archive dirty trees to refs then remove; live-owner and recent-activity protection; `hermes worktree restore`; `worktrees.archive_dirty`, `archive_max_file_mb` 50, `archive_retention_days` 30) | Specified | git-worktrees › Repository registry and sweep schedule (+ Sweep decision table; Archive format; Worktree CLI and restore) |
| TOOL-147 | Subagent worktree isolation (`delegation.worktree_isolation`; one worktree per child `subagent-<id>` on local backend only; clean → removed, work → archived to `refs/hermes/archive/<tree>` with branch kept) | Specified | git-worktrees › Subagent worktree isolation |
| TOOL-148 | Egress proxy with credential injection (iron-proxy: default allowed upstream hosts, SSRF deny CIDRs, bind policy, CA distribution into sandbox, placeholder tokens) | Specified | security-boundaries › Egress proxy coverage of containers and remote sandboxes (+ Egress proxy operation and status) |
| TOOL-149 | Egress proxy credential source and rotation (Bitwarden source, `proxy.credential_source` env\|bitwarden, `allow_env_fallback` false, token rotation slash commands, `proxy.enforce_on_docker`) | Specified | security-boundaries › Egress credential source and placeholder rotation |
| TOOL-150 | Network isolation deployment (Compose topology with internal network + egress proxy; validation steps) | Specified | distribution-and-update › Network-isolated deployment recipe |
| TOOL-151 | Credential pools (fill_first/round_robin/random/least_used, cooldowns, `auth add/list/remove`) | Specified | model-providers › Credential pool selection strategies; Exhausted credential cooldowns; Credential management commands |
| TOOL-152 | Credential pool extras (`auth priority <provider> <target> <n>`, `auth add --priority`, `auth reset` clears cooldowns live, custom-endpoint pools, auto-discovery of several env keys, status reads are peeks, pool sharing with subagents) | Specified | model-providers › Credential pool maintenance commands; › Credential pool seeding, custom endpoint pools and child sharing |
| TOOL-153 | Nous Tool Gateway (one subscription for web search/extract, image gen, TTS, cloud browser, managed Modal; per-category selection keys; mix own keys and gateway (web only); legacy `use_gateway`; self-hosted gateway URL; multiple connector accounts) | Specified | model-providers › Subscription tool gateway routing (connector-account part belongs to connectors) |
| TOOL-154 | Tool output limits (`tool_output.max_bytes` 50000, max_lines 2000, max_line_length 2000) | Specified | execution-environments › Foreground result shaping; tool-system › Result budget and spillover |
| TOOL-155 | `hermes tools` interactive configurator (enable/disable toolsets per platform, pick provider per category: web, image, TTS, browser, video, X search) | Specified | cli › Tool configurator |

## CLI, slash commands and configuration

| ID | Hermes feature | Status | Prometheus requirement |
|---|---|---|---|
| CLI-001 | Binary + default subcommand (`hermes` = `hermes chat`), `--version/-V` | Specified | workspace-architecture › Single binary with subcommand surfaces; workspace-architecture › Version and build identity; cli › Binary names |
| CLI-002 | Global `-p/--profile`, sticky default profile | Specified | cli › Top-level command tree; configuration-and-profiles › Profile selection at process start |
| CLI-003 | `-r/--resume <id\|title\|latest>`, `-c/--continue [name]` (title match, workspace-scoped) | Specified | cli › Resume and continue resolution; cli › Conversation recap on resume |
| CLI-004 | `--in <dir>` (chdir before start; scopes `-c`/`latest` lookup; skips recorded-cwd restore) | Specified | cli › Resume and continue resolution |
| CLI-005 | `--tui` / `--cli` / `--dev` interface selection | Specified | cli › Interface selection is TTY-first; tui › TUI launch selection (`--dev` is contributor tooling, N/A) |
| CLI-006 | `-q/--query` seeds an interactive session (TTY: literal first turn, never parsed as slash/`!`), `--oneshot`, `-Q/--quiet` | Specified | cli › Seed prompt and prompt file |
| CLI-007 | `--query-file PATH` (`-` = stdin; verbatim, mutually exclusive with `-q`) | Specified | cli › Seed prompt and prompt file |
| CLI-008 | `--format stream-json` structured JSONL for one-shot runs, documented exit codes for one-shot | Specified | cli › Streaming JSONL output |
| CLI-009 | `--usage-file PATH` JSON usage report for pipelines | Specified | cli › Usage report file |
| CLI-010 | `-s/--skills` preload skills (repeatable/csv) + `skills.auto_load` config | Specified | skills › Skill preload at launch and per-profile auto-load |
| CLI-011 | `--image <path>` attach image to a one-shot query | Specified | cli › Image attachments |
| CLI-012 | `-w/--worktree` start in an isolated git worktree | Specified | git-worktrees › chat -w worktree mode |
| CLI-013 | `--checkpoints` enable filesystem checkpoints for a run | Specified | execution-environments › Snapshot triggers, caps and per-run toggle |
| CLI-014 | `--yolo` bypass approvals | Specified | tool-system › Approval modes are typed |
| CLI-015 | `--pass-session-id` include session id in system prompt | Specified | agent-turn-loop › Session id in the system prompt |
| CLI-016 | `--ignore-user-config` / `--ignore-rules` | Specified | cli › Ignore profile config and rules |
| CLI-017 | `--safe-mode` (disable config, rules, plugins, hooks, MCP; requires explicit `--model`; isolated worker) | Specified | plugin-system › Safe mode (tightened) |
| CLI-018 | `--provider custom --base-url URL --api-key KEY` launch-only key (memory only) | Specified | model-providers › Launch-only ad-hoc endpoint |
| CLI-019 | `--source <tag>` session source tag (default `cli`, `oneshot` hidden from pickers, `tool` for integrations) | Specified | session-storage › Integration source tag |
| CLI-020 | `--max-turns N` (default 500 / `agent.max_turns`) | Specified | cli › Chat flags (`--max-iterations`); agent-turn-loop › Single documented iteration limit |
| CLI-021 | `-m/--model`, `-t/--toolsets`, `--provider`, `-v/--verbose` | Specified | cli › Chat flags; cli › Top-level command tree (`-v`) |
| CLI-022 | `hermes-agent` legacy single-query runner (`--query`, `--list-tools`, `--save-trajectories`) | Specified | Functionally replaced by cli › One-shot and piped mode; trajectories by observability. No separate binary needed (pmt alias exists). |
| CLI-023 | `!` shell mode in the REPL | Specified | cli › Shell escape in the REPL |
| CLI-024 | Suspend to background (Ctrl+Z) / multi-line Shift+Enter compatibility | Specified | cli › Job-control suspend |
| CLI-025 | Tool progress display modes (off/new/all/verbose), tool preview length, grouping, friendly labels | Specified | cli › Tool progress display and focus view |
| CLI-026 | Background sessions in CLI (`/bg` result panels) | Specified | subagent-delegation › User-launched background sessions |
| CLI-027 | `hermes chat` (subcommand) | Specified | cli › Chat flags |
| CLI-028 | `hermes model` interactive provider/model picker | Specified | model-providers › prometheus model command |
| CLI-029 | `hermes moa list\|configure [name]\|delete <name>` (Mixture-of-Agents presets) | Specified | mixture-of-agents › Preset management commands; › MoA virtual provider and presets |
| CLI-030 | `hermes fallback list\|add\|remove\|clear` | Specified | model-providers › Fallback chain commands |
| CLI-031 | `hermes worktree list\|prune\|restore` | Specified | git-worktrees › Worktree CLI and restore |
| CLI-032 | `hermes browser close-profile` (and browser profile mgmt) | Specified | execution-environments › Real-profile browsing |
| CLI-033 | `hermes secrets bitwarden\|bw setup\|status\|token\|sync [--apply]\|install\|disable` | Specified | configuration-and-profiles › External secret source commands |
| CLI-034 | `hermes secrets onepassword` (op CLI mapping, preview/confirm, caching) | Specified | configuration-and-profiles › External secret source commands |
| CLI-035 | Command secret source (`secrets.command`: run a user command to fetch secrets) | Specified | security-boundaries › Command secret source |
| CLI-036 | `hermes egress install\|setup\|start\|stop\|restart\|reload\|status` (iron-proxy credential-injection firewall) | Specified | security-boundaries › Egress proxy operation and status |
| CLI-037 | `hermes migrate xai [--apply] [--no-backup]` rewrite retired-model references | Specified | configuration-and-profiles › Retired model rewrite |
| CLI-038 | `hermes codex-runtime migrate [--dry-run] [--json]` + `/codex-runtime` | Specified | model-providers › Codex app-server runtime; › Codex runtime migration |
| CLI-039 | `hermes gateway run\|start\|stop\|restart\|status\|install\|uninstall` | Specified | messaging-gateway › One gateway daemon per home (`daemon run/start/stop/restart/status/install-service`) |
| CLI-040 | `hermes gateway list` (all gateways across profiles/homes), `setup`, `migrate-legacy`, `migrate`, `enroll` | Specified | messaging-gateway › Per-OS service installation without elevation; messaging-gateway › Daemon inventory across homes; messaging-gateway › Relay enrollment |
| CLI-041 | `hermes proxy start\|status\|providers` (subscription proxy: local OpenAI-compatible server attaching OAuth creds) | Specified | api-server › Subscription proxy |
| CLI-042 | `hermes lsp status\|list [--installed-only]\|install <id>\|install-all\|restart\|which <id>` | Specified | lsp-diagnostics › LSP CLI |
| CLI-043 | `hermes setup [model\|tts\|terminal\|gateway\|tools\|agent] [--non-interactive] [--reset] [--quick] [--portal]` | Specified | cli › Setup wizard modes |
| CLI-044 | `hermes whatsapp` (Baileys pairing QR flow) / `hermes whatsapp-cloud` | Specified | platform-adapters › WhatsApp pairing and modes; platform-adapters › WhatsApp Cloud API adapter |
| CLI-045 | `hermes slack manifest [--write [PATH]] [--name] [--description] [--long-description(-file)] [--slashes-only]` | Specified | platform-adapters › Slack app manifest generator |
| CLI-046 | `hermes send --to <platform[:chat[:thread]]\|platform:#name> [msg] [--file] [--subject] [--list [platform]] [-q] [--json]` | Specified | agent-interop › One-shot send command |
| CLI-047 | `hermes peer add\|list\|dm\|run\|status\|stop\|remove` (cross-machine bot-to-bot) | Specified | agent-interop › Peer gateway registry; agent-interop › Peer direct messages and remote runs |
| CLI-048 | `hermes portal status\|open\|tools` (Nous Portal + Tool Gateway routing) | Specified | model-providers › Subscription tool gateway routing |
| CLI-049 | `hermes auth add\|list\|remove\|status` | Specified | model-providers › Credential management commands |
| CLI-050 | `hermes auth reset [n]\|priority\|refresh\|logout` + no-arg interactive wizard | Specified | model-providers › Credential pool maintenance commands |
| CLI-051 | `hermes login` / `hermes logout` (deprecated) | N/A | Deprecated aliases of `auth`; no need to carry them. |
| CLI-052 | `hermes status [--full\|--all] [--deep]` | Specified | observability › Configuration-centric status |
| CLI-053 | `hermes usage [--provider] [--json]` account rate-limit windows | Specified | model-providers › Account usage windows command |
| CLI-054 | `hermes cron list\|create\|edit\|pause\|resume\|run\|remove\|status\|runs\|doctor\|tick` | Specified | scheduled-jobs › CLI surface; scheduled-jobs › Cron doctor; scheduled-jobs › Headless tick without a daemon |
| CLI-055 | `hermes cron incidents`, `hermes cron notepad` | Specified | scheduled-jobs › Failure incidents with alert-once and acknowledge; scheduled-jobs › Per-job notepad |
| CLI-056 | `hermes pause [--reason]` / `hermes resume` global emergency stop | Specified | messaging-gateway › Global emergency pause |
| CLI-057 | `hermes webhook subscribe\|list\|remove\|test` dynamic webhook routes | Specified | cli › Webhook subscriptions (tightened: --deliver-only, --script, --cron-job, --mirror-to-session); platform-adapters › Webhook payload filters and transform scripts; platform-adapters › Webhook direct delivery mode |
| CLI-058 | `hermes kanban …` (boards, create, list, show, assign, link, comment, complete, block, dispatch, gc…) | Specified | kanban › CLI surface (detail in kanban slice) |
| CLI-059 | `hermes kanban init\|claim\|context\|specify\|decompose\|request-review\|reopen-review` | Specified | kanban › Additional CLI verbs; kanban › Orchestrator guidance, specify and decompose |
| CLI-060 | `hermes project create\|list\|show\|add-folder\|remove-folder\|rename\|set-primary\|use\|archive\|restore\|bind-board` | Specified | projects › Project records; projects › Project commands; projects › Active project and path ownership; projects › Archive and restore; projects › Kanban board binding |
| CLI-061 | `hermes hooks list\|test <event>\|revoke\|doctor` (shell-script hooks in config with consent allowlist) | Specified | plugin-system › Shell hooks with first-use consent; plugin-system › Hooks CLI |
| CLI-062 | `hermes doctor [--fix]` | Specified | cli › Doctor; workspace-architecture › Doctor |
| CLI-063 | `hermes verify [path] [--detect-only] [--save] [--no-start] [--phase] [--port]` project run-recipe smoke test | Specified | execution-environments › Project run-recipe verification |
| CLI-064 | `hermes security audit [--json] [--fail-on] [--skip-venv\|--skip-plugins\|--skip-mcp]` (OSV.dev supply-chain scan) | Specified | security-boundaries › Dependency vulnerability scan |
| CLI-065 | `hermes approvals suggest [--days 90] [--min-count 2] [--limit 20] [--apply]` / `approvals test` | Specified | tool-system › Approval suggestions mined from history; tool-system › Approval dry-run test command |
| CLI-066 | `hermes dump [--show-keys]` copy-pasteable setup summary | Specified | observability › Plain-text setup summary |
| CLI-067 | `hermes debug share [--lines 200] [--expire 7] [--nous] [--local] [--no-redact]` / `debug delete` | Specified | observability › Opt-in debug share upload |
| CLI-068 | `hermes backup [-o] [--quick] [--label] [--keep 3]` / `hermes import <zip> [--force]` full-home backup/restore | Specified | configuration-and-profiles › Whole-home backup; configuration-and-profiles › Home restore |
| CLI-069 | `hermes checkpoints [status\|list\|prune\|clear\|clear-legacy] [--limit 20] [--retention-days 7] [--max-size-mb 500] [--keep-orphans] [-f]` | Specified | execution-environments › Snapshot store management CLI |
| CLI-070 | `hermes logs [agent\|errors\|gateway\|gui\|desktop\|mcp\|update\|handoff\|list] [-n] [-f] [--level] [--session] [--since] [--component]` | Specified | cli › Log viewer |
| CLI-071 | `hermes prompt-size [--platform] [--json]` offline byte breakdown of system prompt + tool schemas | Specified | agent-turn-loop › Offline prompt size report |
| CLI-072 | `hermes config show\|edit\|get\|set\|unset\|path\|env-path\|check\|migrate` | Specified | configuration-and-profiles › Config CLI; › Config schema versioning and migration |
| CLI-073 | `hermes skin list\|use\|set` | Specified | cli › Theme commands and branding keys |
| CLI-074 | `hermes console` curated safe command REPL | Specified | cli › Safe command console |
| CLI-075 | `hermes pairing list\|approve\|revoke\|clear-pending` | Specified | messaging-gateway › Documented pairing flow (`clear-pending` ≈ `deny` of all pending) |
| CLI-076 | `hermes skills install\|list\|reset\|diff\|trust` | Specified | skills › Hub install pipeline and trust policy; › Project-local skills require explicit trust; › Skills lockfile |
| CLI-077 | `hermes skills browse\|search [--source]\|inspect\|check\|update\|audit\|uninstall\|untrust` | Specified | skills › Hub discovery, inspection, updates and audit |
| CLI-078 | `hermes skills opt-out [--remove]\|opt-in [--sync]\|repair-official\|list-modified` | Specified | skills › Bundled skill opt-out and official catalog repair |
| CLI-079 | `hermes skills publish\|snapshot export\|import\|tap list\|add\|remove\|config` | Specified | skills › Skill publishing and snapshots; skills › Custom skill taps; skills › Interactive per-surface skill enablement |
| CLI-080 | `hermes bundles list\|show\|create [--skill…] [--description] [--instruction]\|delete\|reload` + `/bundles` + `/<bundle>` | Specified | skills › Skill bundles |
| CLI-081 | `hermes plugins install\|search\|update\|remove\|enable\|disable\|list\|doctor` | Specified | plugin-system › plugins CLI |
| CLI-082 | `hermes plugins adopt\|trust-update-url\|check-updates\|capabilities\|pack install\|pack export\|pack show` | Specified | plugin-system › plugins CLI (tightened: adopt, capabilities, check-updates/update with no id); plugin-system › Plugin collections |
| CLI-083 | `hermes curator status\|run\|pause\|resume\|pin\|unpin\|restore\|archive\|prune\|list-archived\|backup\|rollback` | Specified | skills › Curator CLI |
| CLI-084 | `hermes pets list\|install\|select\|show\|off\|scale\|remove\|doctor` | Specified | pets › Pet gallery commands; pets › Pet preview and doctor; pets › Pet configuration and scale |
| CLI-085 | `hermes journey` (aliases learning, memory-graph) timeline of learned skills + memories | Specified | memory › Learning journey timeline; Journey pruning and editing |
| CLI-086 | `hermes memory setup\|status\|off\|reset` | Specified | memory › Memory CLI and editing |
| CLI-087 | `hermes tools [--summary]` / `tools list\|disable\|enable\|post-setup` (per-platform tool enablement) | Specified | tool-system › Tool configuration summary and post-setup (enable/disable per surface already in cli › Tool configurator) |
| CLI-088 | `hermes computer-use install [--upgrade]\|status\|doctor\|permissions status\|grant` and `screen status\|start\|stop\|install` | Specified | computer-use › Computer-use CLI and doctor (+ Platform prerequisites; screen subcommands in execution-environments › Bot screen) |
| CLI-089 | `hermes mcp add\|remove\|list\|test\|configure\|login\|reauth\|picker\|catalog\|install` | Specified | plugin-system › mcp CLI |
| CLI-090 | `hermes connectors list\|accounts\|connect\|rename\|disconnect` + TUI `/connectors` | Specified | connectors › Connector account management CLI |
| CLI-091 | `hermes sessions list\|export\|delete\|prune\|archive\|stats\|rename\|pin\|unpin\|import` | Specified | session-storage › Session management CLI; › Portable export and import |
| CLI-092 | `hermes sessions browse` interactive picker (status tags done/intr/err/empty, msg count, search, resume) | Specified | session-storage › Interactive session browser |
| CLI-093 | `hermes sessions optimize\|optimize-storage\|repair\|recover\|set-journal-mode` | Specified | session-storage › Offline recovery and journal mode |
| CLI-094 | `hermes sessions repair-routing\|repair-prompts\|refresh-prompts\|repair-profiles\|retitle-skills\|clean-markers\|pinned` | Specified | session-storage › Prompt refresh and retitling |
| CLI-095 | Session export formats: JSONL, HTML, prompts-only, traces (HF Agent Trace Viewer), Markdown/QMD | Specified | session-storage › Export formats and save command |
| CLI-096 | Import sessions from Claude Code / Codex CLI | Specified | hermes-migration › Session transcripts from other agent CLIs |
| CLI-097 | `hermes insights [--days 30] [--source platform]` | Specified | observability › Usage insights report |
| CLI-098 | `hermes monitoring status` (gateway health/diagnostics export + redaction posture) | Specified | observability › Export posture status |
| CLI-099 | `hermes vault add\|list\|rm\|sources [--enable\|--disable]` local encrypted autofill vault | Specified | security-boundaries › Vault management CLI |
| CLI-100 | `hermes update [--check] [--plan] [--backup\|--no-backup] [--yes] [--set-channel] [--channel] [--branch] [--install-id]` | Specified | distribution-and-update › Release channels; › Update exit codes; › Update dry-run and changelog preview; › Install receipt; › Running daemon handover; › Update receipts at the root home |
| CLI-101 | `hermes uninstall [--full] [--gui] [--data] [--dry-run] [--yes]` | Specified | distribution-and-update › Uninstall (`--purge`; `--dry-run` listing implied) |
| CLI-102 | `hermes acp` | Specified | acp-adapter › Launch entry point and pre-server flags |
| CLI-103 | `hermes profile list\|use\|create\|delete\|show\|rename\|export\|import\|alias` | Specified | configuration-and-profiles › Profile lifecycle commands; › Safe profile deletion; › Profile alias launchers; › Profile export excludes credentials by default |
| CLI-104 | `profile create --clone\|--clone-all\|--no-skills\|--no-alias\|--description\|--clone-channels\|--sync-imports` | Specified | configuration-and-profiles › Profile creation options |
| CLI-105 | `hermes profile describe [--text\|--auto [--overwrite] [--all]]` | Specified | configuration-and-profiles › Profile descriptions |
| CLI-106 | `hermes profile install\|update\|info` (profile distributions from git) | Specified | configuration-and-profiles › Profile distributions |
| CLI-107 | `hermes profile migrate-identity\|purge-identity` | Specified | configuration-and-profiles › Identity migration and purge |
| CLI-108 | `hermes completion bash\|zsh\|fish` | Specified | cli › Shell completions (upgrade) |
| CLI-109 | `hermes dashboard [--host] [--port] [--no-open] [--status] [--stop] [--tui] [--insecure]` | Specified | web-dashboard › Dashboard command and listener defaults; › Non-loopback binds fail closed |
| CLI-110 | `hermes dashboard register` (register dashboard with portal / OAuth redirect) | Specified | web-dashboard › Registering a dashboard with the account portal |
| CLI-111 | `hermes serve` headless backend (powers desktop + remote backends) | Specified | messaging-gateway › One gateway daemon per home (`daemon run`) |
| CLI-112 | `hermes webapp [--no-open] [--build-only] [--skip-build] [--status] [--sign-out]` Desktop workspace in a browser | Specified | web-dashboard › Desktop workspace in the browser; web-dashboard › Webapp sign-out and insecure-context notice |
| CLI-113 | `hermes desktop` (alias gui) build + launch desktop app | Specified | desktop-app › Launching the desktop from the CLI |
| CLI-114 | `hermes pm doctor\|status\|repair\|install [chromium]` | Specified | distribution-and-update › Runtime CLI |
| CLI-115 | `hermes import-agent openclaw\|claude-code\|codex\|cursor` / `hermes claw migrate\|cleanup` (agent-importers plugin) | Specified | hermes-migration › Importers for other agents; OpenClaw mapping; Source archiving after import; Keeping imports in sync |
| CLI-116 | Plugin-registered CLI subcommands (e.g. `hermes honcho`, `hermes spotify`) | Specified | plugin-system › Plugin CLI subcommands |
| CLI-117 | `/start` (gateway) silent ack of platform start pings | Specified | messaging-gateway › Platform start pings and paginated command browser |
| CLI-118 | `/new [name]` (alias `/reset`), inline skip `now\|--yes\|-y` | Specified | cli › Built-in slash command set; cli › Destructive command confirmation (optional `[name]` initial title is a minor gap) |
| CLI-119 | `/topic [off\|help\|session-id]` Telegram DM multi-session topic mode | Specified | messaging-gateway › Multi-session DM topics |
| CLI-120 | `/clear` | Specified | cli › Built-in slash command set; › Destructive command confirmation |
| CLI-121 | `/redraw` force full UI repaint | Specified | tui › Client-local display commands |
| CLI-122 | `/history` show conversation history (respects timestamps) | Specified | cli › Local transcript and session commands |
| CLI-123 | `/save <json\|md\|html> [filename] [redact]` export current conversation | Specified | session-storage › Export formats and save command |
| CLI-124 | `/retry` resend last user message | Specified | cli › Retry semantics |
| CLI-125 | `/prompt` (alias `/compose`) compose next prompt in `$EDITOR` | Specified | cli › External editor composition |
| CLI-126 | `/undo [N]` | Specified | cli › Built-in slash command set; session-storage › Rewind is a soft delete; execution-environments › File snapshots and undo |
| CLI-127 | `/title [name]` | Specified | session-storage › Session titles with provenance |
| CLI-128 | `/handoff <platform>` hand CLI session to a messaging platform | Specified | messaging-gateway › Session handoff to a messaging platform |
| CLI-129 | `/branch [--here] [name]` (alias `/fork`) | Specified | messaging-gateway › Branching into a sibling thread |
| CLI-130 | `/worktree [new [name]\|list\|prune [--dry-run]]` | Specified | git-worktrees › In-session worktree command |
| CLI-131 | `/compress [here [N] \| focus \| --preview]` (alias `/compact`) | Specified | context-compression › Manual compaction command (alias `/compact` should be added) |
| CLI-132 | `/rollback [number] [--all]`, `/rollback diff <N>` | Specified | execution-environments › Rollback listing, diff and selective restore |
| CLI-133 | `/snapshot [create [label]\|restore <id>\|prune [N]]` (alias `/snap`) | Specified | configuration-and-profiles › State snapshots |
| CLI-134 | `/export [profile] [-o]`, `/import <archive> [--name]` | Specified | cli › Exit with delete and profile archive slash forms |
| CLI-135 | `/stop` | Specified | cli › Busy policy enforcement; messaging-gateway › Stop preserves the session |
| CLI-136 | `/discard` drop a turn lost in a restart so queued messages run | Specified | messaging-gateway › Discarding a restart-interrupted turn |
| CLI-137 | `/pause [reason \| off]` (gateway) | Specified | messaging-gateway › Global emergency pause |
| CLI-138 | `/approve [session\|always]`, `/deny [all] [reason]` (gateway) | Specified | messaging-gateway › Text approval and busy-mode commands |
| CLI-139 | `/bg <prompt>` run prompt in a separate background session; result delivered to same chat | Specified | subagent-delegation › User-launched background sessions |
| CLI-140 | `/btw <question>` side question from transcript snapshot | Specified | agent-turn-loop › Side questions with /btw |
| CLI-141 | `/agents` (alias `/tasks`) active agents and running tasks | Specified | subagent-delegation › Agents monitor and live transcripts |
| CLI-142 | `/journey [list\|delete\|edit]` (aliases `/learning`, `/memory-graph`) | Specified | memory › Journey pruning and editing |
| CLI-143 | `/queue [<prompt>\|list\|edit N\|rm N\|move A B\|clear\|add]` (alias `/q`) | Specified | cli › Queue management commands |
| CLI-144 | `/steer <prompt>` (alias `/s`) | Specified | agent-turn-loop › Structured steering (upgrade); › Steer placement preserves the cache prefix |
| CLI-145 | `/goal [text\|draft\|show\|gate add\|pause\|resume\|clear\|status\|wait <pid>\|unwait]` standing goals | Specified | session-automation › Goal command surface |
| CLI-146 | `/subgoal [text\|remove N\|clear]` | Specified | session-automation › Subgoals |
| CLI-147 | `/heartbeat every <interval> <prompt>\|status\|pause\|resume\|clear` (alias `/hb`) | Specified | session-automation › Session heartbeat |
| CLI-148 | `/loop [interval] <prompt> [--times N] [--until cond]\|status\|pause\|resume\|stop` (alias `/proactive`) | Specified | session-automation › Loop command and cadence; Loop stop conditions |
| CLI-149 | `/refine [focus]` run memory/skill self-improvement review now | Specified | skills › /refine runs self-improvement review on demand |
| CLI-150 | `/review [instructions]` independent reviewer subagent; result re-enters session | Specified | subagent-delegation › Review command |
| CLI-151 | `/plan [task]` write implementation plan to `.hermes/plans/`, no execution | Specified | cli › Planning-only turn |
| CLI-152 | `/moa <prompt>` one prompt through default MoA preset | Specified | mixture-of-agents › One-shot /moa command |
| CLI-153 | `/status` session info + local "Session recap" | Specified | cli › Local transcript and session commands |
| CLI-154 | `/egress [status]` | Specified | security-boundaries › Egress proxy operation and status |
| CLI-155 | `/context [all]` (alias `/ctx`) context window gauge + category breakdown | Specified | context-compression › Context window view |
| CLI-156 | `/whoami` | Specified | messaging-gateway › Slash command access tiers |
| CLI-157 | `/profile` | Specified | cli › Built-in slash command set |
| CLI-158 | `/sethome` (alias `/set-home`) set chat as home channel | Specified | messaging-gateway › Home channel and /sethome |
| CLI-159 | `/resume [name]` (bare lists named sessions; refused mid-turn in CLI) | Specified | cli › Built-in slash command set; session-storage › Title lineage numbering and resolution |
| CLI-160 | `/sessions [all] [search <q>]` (TUI: live session switcher, `/switch`) | Specified | cli › Local transcript and session commands |
| CLI-161 | `/config` | Specified | cli › Built-in slash command set |
| CLI-162 | `/model [model] [--provider] [--reasoning] [--global\|--session] [--refresh]`, aliases, `@preset` | Specified | model-providers › /model switch command (user model aliases `model.aliases.*` with own base_url/key are a gap to add) |
| CLI-163 | `/codex-runtime [auto\|codex_app_server]` | Specified | model-providers › Codex app-server runtime |
| CLI-164 | `/personality [name\|none]` | Specified | agent-turn-loop › Personality overlays |
| CLI-165 | `/statusbar` (alias `/sb`) toggle status bar | Specified | tui › Client-local display commands |
| CLI-166 | `/battery [on\|off\|status]` | Specified | tui › Client-local display commands |
| CLI-167 | `/timestamps [on\|off\|status]` (alias `/ts`) | Specified | cli › Display preferences |
| CLI-168 | `/diff [staged\|all\|session] [--stat] [path...]` | Specified | execution-environments › Working-tree diff command |
| CLI-169 | `/verbose` cycle tool progress | Specified | cli › Tool progress display and focus view |
| CLI-170 | `/focus [on\|off\|status]` display-only reduced output | Specified | cli › Tool progress display and focus view |
| CLI-171 | `/footer [on\|off\|status]` gateway runtime-metadata footer (model, context %, cwd) | Specified | messaging-gateway › Runtime metadata footer |
| CLI-172 | `/yolo` | Specified | tool-system › Approval modes are typed |
| CLI-173 | `/approvals [manual\|smart\|off]` | Specified | cli › Built-in slash command set; tool-system › Approval modes are typed |
| CLI-174 | `/reasoning [level\|show\|hide\|full\|clamp] [--global]` (levels none…max, ultra) | Specified | model-providers › Reasoning effort defaults, overrides and display |
| CLI-175 | `/fast [normal\|fast\|auto\|cold\|ultrafast\|status] [--global]` priority processing | Specified | model-providers › Fast mode service tier |
| CLI-176 | `/skin [name]` | Specified | cli › Themes are data (`/theme`) |
| CLI-177 | `/indicator [kaomoji\|emoji\|unicode\|ascii]` busy indicator style | Specified | tui › Busy indicator styles |
| CLI-178 | `/voice [on\|off\|tts\|join\|channel\|leave\|status]` | Specified | voice-mode › Voice mode toggle and commands; voice-mode › Gateway per-chat voice reply command; voice-mode › Discord voice channel sessions |
| CLI-179 | `/wake [on\|off\|status]` "Hey Hermes" wake word | Specified | voice-mode › Wake word listener; voice-mode › Wake engines and false-trigger control |
| CLI-180 | `/busy [queue\|steer\|interrupt\|status]` | Specified | messaging-gateway › Text approval and busy-mode commands |
| CLI-181 | `/autoprune [on\|off\|later\|status]` + low-disk prompt | Specified | session-storage › Low-disk prune prompt |
| CLI-182 | `/tools [list\|disable\|enable] [name...]` | Specified | cli › Deferred effects with `--now` |
| CLI-183 | `/toolsets` list toolsets | Specified | tool-system › Toolset listing commands |
| CLI-184 | `/skills` (search/install/inspect) + skill write-approval review `pending\|approve\|reject\|diff\|approval on\|off` | Specified | skills › Skill write-approval review surface |
| CLI-185 | `/memory [pending\|approve\|reject\|approval]` | Specified | memory › Write approval and pending changes |
| CLI-186 | `/bundles` | Specified | skills › Skill bundles |
| CLI-187 | `/pet [toggle\|list\|scale <n>\|<slug>]` | Specified | pets › Pet slash command |
| CLI-188 | `/hatch [description]` (alias `/generate-pet`) | Specified | pets › Generating a pet |
| CLI-189 | `/learn <what to learn from>` distill a skill from dirs/URLs/chat/notes | Specified | skills › /learn distils a skill from sources |
| CLI-190 | `/init [notes]` generate/update AGENTS.md from repo scan | Specified | agent-turn-loop › Project instructions bootstrap with /init |
| CLI-191 | `/cron [subcommand]` | Specified | scheduled-jobs › /cron slash command |
| CLI-192 | `/suggestions [accept\|dismiss N\|catalog\|clear]` (alias `/suggest`) suggested automations | Specified | scheduled-jobs › Suggested automations inbox |
| CLI-193 | `/blueprint [name] [slot=value ...]` (alias `/bp`) | Specified | scheduled-jobs › Automation blueprint catalog |
| CLI-194 | `/curator [status\|run\|pin\|archive…]` | Specified | skills › /curator slash command |
| CLI-195 | `/kanban <action>` (same surface as CLI, bypasses busy guard, auto-subscribe on create) | Specified | kanban › /kanban slash command |
| CLI-196 | `/reload` reload `.env` into running session | Specified | configuration-and-profiles › Secrets are resolved per use |
| CLI-197 | `/reload-mcp` (alias `/reload_mcp`) | Specified | plugin-system › MCP client servers (`/mcp reload`) |
| CLI-198 | `/reload-skills` re-scan skills dir | Specified | skills › /reload-skills rescans roots |
| CLI-199 | `/browser [connect\|disconnect\|status]` live Chromium via CDP (default 127.0.0.1:9222, auto-launch) | Specified | execution-environments › Connecting to the user's browser |
| CLI-200 | `/plugins` list plugins | Specified | plugin-system › plugins CLI (tightened: /plugins slash command) |
| CLI-201 | `/commands [page]` (gateway) paginated commands + skills | Specified | messaging-gateway › Platform start pings and paginated command browser |
| CLI-202 | `/help [skills\|<filter>]` | Specified | cli › Built-in slash command set (filter + `skills` sub-list minor gap) |
| CLI-203 | `/palette` fuzzy command palette (Ctrl+P) | Specified | tui › Fuzzy command palette |
| CLI-204 | `/restart` (gateway) graceful restart after drain, confirmation to requester | Specified | messaging-gateway › Restart and update from chat |
| CLI-205 | Persistent goals feature (judge model, completion contracts, quality gates, park on background pid, `goals.max_turns` 20, preemption by user messages, persistence across resume) | Specified | session-automation › Goal judge loop; Goal turn budget and blocked verdict; Idle-boundary arbitration |
| CLI-206 | Loops feature (`/loop`, fixed vs self-paced cadence, `--times`, `--until` judge; `loops.min_interval_seconds` 30, `max_ticks` 100, self-paced floor 60 / ceiling 900) | Specified | session-automation › Loop command and cadence; Loop stop conditions; Idle-boundary arbitration |
| CLI-207 | Session heartbeats (`/heartbeat`, min 60 s, missed ticks coalesce, idle-only, in-process) | Specified | session-automation › Session heartbeat; Heartbeat ownership, recovery and silence on the gateway |
| CLI-208 | Voice mode + wake word (record key Ctrl+B, max 120 s, silence detection, barge-in, stop phrases, auto TTS; wake phrase "hey hermes", providers openwakeword/sherpa/porcupine) | Specified | voice-mode › Voice mode toggle and commands; › Push-to-talk recording and continuous loop; › Two-stage silence detection; › Spoken and typed stop phrases; › Barge-in; › Wake word listener |
| CLI-209 | Pets (petdex animated mascots across CLI/TUI/desktop; gallery install, select, scale, `/hatch` image generation) | Specified | pets › Pets are cosmetic and cache-neutral; pets › Pet storage and sprite format; pets › One activity-to-state mapping; pets › Terminal pet rendering; pets › Affection reactions |
| CLI-210 | Skins: built-in skins, YAML keys (colors, spinner, branding), custom skins, desktop customCSS | Specified | cli › Theme commands and branding keys |
| CLI-211 | Personality / SOUL.md (identity file, built-in personalities, custom `personalities:` config, security scanning) | Specified | agent-turn-loop › Personality overlays |
| CLI-212 | Language packs / UI i18n (`display.language`, 17 bundled locales, plugin `provides_locales`, overlay `<home>/locales/<lang>.yaml`, partial layers, validation) | Specified | i18n › Static UI string catalogs; Layered lookup with partial catalogs; Language-pack plugins; Language pack validation |
| CLI-213 | Profile distributions (git-based `distribution.yaml`, install/update/info/remove, distribution-owned vs user-owned files, env var prompts, tags) | Specified | configuration-and-profiles › Profile distributions |
| CLI-214 | Managed scope (`/etc/hermes/config.yaml` + `.env`, root-owned, per-key override, relocatable dir) | Specified | configuration-and-profiles › Managed secrets and managed visibility |
| CLI-215 | Remote config mode (config plane: global/tenant/group/owner/agent layering, locked keys, fail-closed startup fetch, instance id) | Specified | configuration-and-profiles › Remote configuration backend |
| CLI-216 | Session naming: auto-generated titles, title rules, `/title` in messaging | Specified | session-storage › Session titles with provenance; › Title lineage numbering and resolution |
| CLI-217 | Git worktree feature page (parallel agents, safe cleanup, `-w` auto mode, stale sweep/restore) | Specified | git-worktrees › Sweep decision table (+ Archive format; Repository registry and sweep schedule; Exit disposition) |
| CLI-218 | Quick commands | Specified | cli › Quick commands; messaging-gateway › Gateway slash commands, quick commands and hooks |
| CLI-219 | Skill slash commands (`/<skill>`, `/skill <name>` fallback when shadowed) | Specified | skills › Skill slash commands; cli › Skill invocation as user message |
| CLI-220 | Slack `!` command prefix inside threads | Specified | platform-adapters › Slack slash commands and thread command prefix |
| CLI-221 | Slash access split `allow_admin_from` / `user_allowed_commands` (+ group variants) | Specified | messaging-gateway › Slash command access tiers |
| CLI-222 | Command prefix matching (ambiguous → first in registry order) | Specified | cli › Command resolution (Prometheus lists candidates instead — deliberate) |
| CLI-223 | Config `model`, `providers`, `fallback_providers`, `fallback`, `credential_pool_strategies`, `model_overrides`, `openrouter`, `bedrock`, `vertex`, `nous` | Specified | model-providers › Typed profile-scoped provider registry; › Fallback chain configuration; › Credential pool selection strategies (bedrock region/guardrail, vertex project/region detail in providers slice) |
| CLI-224 | Config `model.aliases` (user model aliases with base_url/api_key/key_env) | Specified | model-providers › User model aliases |
| CLI-225 | Config `toolsets` (default `["hermes-cli"]`), `tools.tool_search`, `tools.connectors`, `agent.disabled_toolsets` | Specified | tool-system › Built-in toolsets and composition; › Toolset selection is validated; › Tool search progressive disclosure |
| CLI-226 | Config `database` (journal_mode wal, wal_autocheckpoint, journal_size_limit) | Specified | session-storage › WAL mode, bounded readers and checkpoints |
| CLI-227 | Config `runtime.nofile_soft_limit` (4096) | Specified | workspace-architecture › File-descriptor soft limit |
| CLI-228 | Config `max_concurrent_sessions` (None), `max_live_sessions` (16) | Specified | messaging-gateway › Concurrency caps across surfaces |
| CLI-229 | Config `session.terminal_continue` (per-terminal continue) | Specified | cli › Resume and continue resolution |
| CLI-230 | Config `attachments.storage` (hermes-home) | Specified | session-storage › Attachment storage location |
| CLI-231 | Config `agent.*` (max_turns, budget_warning_ratio, run_budget_seconds, api_max_retries, auto_recovery_cycles, service_tier, fast_auto_seconds, tool_use_enforcement, verify_on_stop, clarify_timeout, image_input_mode, reasoning_overrides, gateway_* timeouts…) | Specified | agent-turn-loop › Wall-clock run budget; Verify-on-stop; Coding posture and standing coding instructions; Empty-response and narration guards (session stall watchdog + reconnect attention belong to messaging-gateway) |
| CLI-232 | Config `terminal.*` | Specified | execution-environments › Terminal configuration defaults (detail in tools slice) |
| CLI-233 | Config `web.*` (backend, search/extract backend, extract_char_limit, keyless fallback, cache) | Specified | web-and-media-tools › Web backend selection precedence; › Keyless web search rotation is explicit opt-in; › Web result caching; › web_extract tool contract |
| CLI-234 | Config `browser.*` | Specified | execution-environments › Browser session resolution; › Local browser isolation |
| CLI-235 | Config `worktrees.*` | Specified | git-worktrees › Sweep decision table (+ Archive format; Shared install state retention; Worktree creation) |
| CLI-236 | Config `checkpoints.*` (enabled false, max_snapshots 20, max_total_size_mb 500, max_file_size_mb 10, auto_prune, retention_days 7, min_interval_hours 24) | Specified | execution-environments › Snapshot triggers, caps and per-run toggle |
| CLI-237 | Config `context_file_max_chars`, `context_file_read_timeout` (5 s), `file_read_max_chars` (100000) | Specified | agent-turn-loop › Context file read timeout |
| CLI-238 | Config `mcp_discovery_timeout` (1.5 s), `mcp_single_query_discovery_timeout` (15 s), `mcp.*` | Specified | plugin-system › MCP config watch, recycling and discovery wait |
| CLI-239 | Config `tool_output` (max_bytes 50000, max_lines 2000, max_line_length 2000) | Specified | execution-environments › Foreground result shaping; tool-system › Result budget and spillover |
| CLI-240 | Config `tool_loop_guardrails` (warn_after exact_failure 2 / same_tool_failure 3 / idempotent_no_progress 2; hard_stop_after 5/8/5; non-interactive hard stop on; loop_caps max_web_searches 50, max_subagents 50) | Specified | agent-turn-loop › Tool loop guardrails |
| CLI-241 | Config `compression.*`, `context.engine`, `prompt_caching.cache_ttl` | Specified | context-compression › Compression configuration schema; › Context engine selection; › Prompt cache TTL |
| CLI-242 | Config `auxiliary.*` (per-task aux model routing incl. review, profile_describer, mcp) | Specified | model-providers › Auxiliary task routing |
| CLI-243 | Config `display.*` (compact, interface, cli_layout, bell_on_complete/prompt, show_reasoning, show_cost, final_response_markdown, persistent_output, inline_diffs, vim_mode, user_message_preview, status_phrases, turn_summary, copy_shortcut, language, skin…) | Specified | cli › Display preferences |
| CLI-244 | Config `display.platforms.<platform>.*` per-platform display overrides, `display.runtime_footer` | Specified | messaging-gateway › Display policy and per-platform overrides |
| CLI-245 | Config `webapp` (enabled false, host 127.0.0.1, port 9120, public_url) | Specified | web-dashboard › Desktop workspace in the browser (webapp.enabled, webapp.public_url; host/port shared with dashboard listener) |
| CLI-246 | Config `dashboard.*` (theme, trusted_proxies, ws ping, oauth, basic_auth, session_token_auth, public_url, show_token_analytics) | Specified | web-dashboard › Dashboard command and listener defaults; › Reverse-proxy base path; › Loopback authentication (OAuth/basic auth options for remote dashboard detail in dashboard slice) |
| CLI-247 | Config `privacy.redact_pii` (false) | Specified | security-boundaries › Optional PII redaction |
| CLI-248 | Config `tts.*`, `stt.*` (providers edge/elevenlabs/openai/gemini/xai/mistral/minimax/kittentts/neutts/piper/deepinfra; local whisper/groq…) | Specified | web-and-media-tools › Built-in text-to-speech providers; › Built-in speech-to-text providers and resolution |
| CLI-249 | Config `voice.*`, `wake_word.*` | Specified | voice-mode › Push-to-talk recording and continuous loop; › Two-stage silence detection; › Wake engines and false-trigger control; › Single wake listener ownership across surfaces; › Wake capture location for remote clients |
| CLI-250 | Config `vision.*` (embed_target_bytes 262144, max_calls_per_image, replay_recent_images 3) | Specified | web-and-media-tools › Image embed budget, per-image cap and replay |
| CLI-251 | Config `human_delay` (mode off, min 800 ms, max 2500 ms) | Specified | platform-adapters › Typing indicator and human delay |
| CLI-252 | Config `memory.*` | Specified | memory › Core tier character budgets; › Memory nudge interval; › Write approval |
| CLI-253 | Config `delegation.*`, `code_execution.*` | Specified | subagent-delegation › Delegation configuration defaults; › execute_code RPC channel and limits |
| CLI-254 | Config `prefill_messages_file` | Specified | agent-turn-loop › Prefill messages |
| CLI-255 | Config `goals.max_turns` (20), `loops.*` | Specified | session-automation › Automation configuration |
| CLI-256 | Config `moa.*` | Specified | mixture-of-agents › MoA virtual provider and presets; › Privacy filter for advisor outputs; › MoA traces |
| CLI-257 | Config `skills.*`, `curator.*` | Specified | skills › Curator configuration and scheduling; › Hub install pipeline (`skills.write_approval`, `auto_load` gaps noted in CLI-184/CLI-010) |
| CLI-258 | Config `honcho` (memory provider) | Specified | memory › External memory provider contract |
| CLI-259 | Config `timezone` | Specified | scheduled-jobs (timezone for schedules); also needs prompt-time timestamp use (agent slice) |
| CLI-260 | Config `slack`, `discord`, `whatsapp`, `telegram`, `mattermost`, `matrix` top-level platform sections | Specified | platform-adapters › Platform configuration shape (under `gateway.platforms.<name>`) |
| CLI-261 | Config `approvals.*` (mode, timeout, destructive_slash_confirm, cron_mode…) | Specified | tool-system › Approval modes are typed; cli › Destructive command confirmation (`cli.confirm_destructive`) |
| CLI-262 | Config `command_allowlist` | Specified | tool-system › Human approval choices and grants file (`[approvals].allow` in grants.toml) |
| CLI-263 | Config `quick_commands` | Specified | cli › Quick commands |
| CLI-264 | Config `platform_hints` | Specified | agent-turn-loop › Platform hint overrides |
| CLI-265 | Config `plugins.*` | Specified | plugin-system › Opt-in enablement; › Safe mode |
| CLI-266 | Config `hooks`, `hooks_auto_accept` (shell hooks) | Specified | plugin-system › Shell hooks with first-use consent |
| CLI-267 | Config `personalities` | Specified | agent-turn-loop › Personality overlays |
| CLI-268 | Config `auth.*` (adopt_external_logins, codex_login_flow) | Specified | model-providers › Borrowed CLI logins |
| CLI-269 | Config `security.*` (allow_private_urls, redact_secrets, website_blocklist, protected_instruction_files, acked_advisories, allow_lazy_installs…) | Specified | security-boundaries › Protected instruction files configuration (+ Website blocklist; Supply-chain advisory catalog; In-process fetch SSRF protection for fake_ip_ranges; allow_lazy_installs = distribution-and-update runtimes.allow_lazy_install) |
| CLI-270 | Config `cron.*`, `kanban.*` | Specified | scheduled-jobs; kanban (detail in their slices) |
| CLI-271 | Config `bot_mode` (envelope_ttl_seconds 900, turn_wait_seconds 120) + Bot Chats | Specified | bot-mode › message_agent tool; bot-mode › Typed delivery failures and safe retry |
| CLI-272 | Config `logging` (level INFO, max_size_mb 5, backup_count 3) | Specified | cli › Log viewer (rotation 10 MiB × 5); observability › Structured logs |
| CLI-273 | Config `model_catalog` (enabled, url, ttl_minutes 20, providers), `models_dev.url` | Specified | model-providers › Model metadata catalog |
| CLI-274 | Config `network.force_ipv4` | Specified | model-providers › Outbound IPv4 preference |
| CLI-275 | Config `monitoring.*` (gateway_health_export, export.otlp) | Specified | messaging-gateway › Gateway health export is opt-in; observability › Opt-in OpenTelemetry export |
| CLI-276 | Config `hosted_rooms.discussion` (max_rounds 3, max_messages 10) | Specified | bot-mode › Group chats |
| CLI-277 | Config `gateway.*`, `streaming.*` | Specified | messaging-gateway; messaging-gateway › Streaming to platforms |
| CLI-278 | Config `sessions.*` (auto_prune, retention_days, auto_archive(_days), show_subagents, vacuum_after_prune, cjk_fts, max_resume_messages, max_export_messages) | Specified | session-storage › Retention, pruning and auto-archive; › Resume and export size guards; › Full-text session search |
| CLI-279 | Config `onboarding` (seen tips, profile_build ask, run) | Specified | cli › Onboarding state |
| CLI-280 | Config `telemetry.shared_metrics` (off) | Specified | security-boundaries › Telemetry is opt-in and HTTPS-only |
| CLI-281 | Config `doctor.live_probe_timeout` (10) | Specified | cli › Doctor (`--online`) — timeout default to add |
| CLI-282 | Config `updates.*` (check, pre_update_backup quick\|full\|off, backup_keep 5, non_interactive_local_changes) | Specified | distribution-and-update › Update check without telemetry; › Schema migrations around updates (git-stash keys N/A for binary installs) |
| CLI-283 | Config `lsp.*` | Specified | lsp-diagnostics › Wait modes and timeouts (+ Workspace gating; Server installation; Server overrides and custom servers; Workspace trust) |
| CLI-284 | Config `x_search` (model grok-4.5, timeout 180, retries 2) | Specified | web-and-media-tools › x_search tool |
| CLI-285 | Config `vault.*` | Specified | security-boundaries › Credential vault storage (+ Password manager login sources) |
| CLI-286 | Config `secrets.bitwarden`, `secrets.onepassword` | Specified | security-boundaries › External secret sources are opt-in |
| CLI-287 | Config `paste_collapse_threshold` (5 lines), `paste_collapse_char_threshold` (2000) | Specified | tui › Large pastes collapse into chips |
| CLI-288 | Config `bot_desktop` (geometry 1440x900, auto_start, min_free_memory_mb 1536, idle_stop_minutes 30) | Specified | execution-environments › Bot screen (bot_desktop.* keys; routing: Bot Screen → execution-environments) |
| CLI-289 | Config `computer_use.*` | Specified | computer-use › Capture modes and payload budget (+ Permission modes are fixed per runtime; Driver runtime management and identity; Platform prerequisites; Agent cursor overlay; Backend selection and availability) |
| CLI-290 | Config `proxy.*` (egress) | Specified | security-boundaries › Egress proxy operation and status (+ Egress credential source and placeholder rotation; Egress proxy coverage of containers and remote sandboxes) |
| CLI-291 | Config `desktop.*` (font_family, repo_scan, electron_flags, disable_gpu, password_store, auto_continue…) | Specified | desktop-app › Desktop launch options; desktop-app › Repository discovery for the Projects sidebar; desktop-app › Fonts, shortcuts, palette and profile files |
| CLI-292 | Config `local_runtime` (enabled false, backend auto, models_max 4, port, detect_ports) | Specified | local-models › Local runtime configuration; › External llama-server detection |
| CLI-293 | Config `_config_version` (52) schema versioning | Specified | configuration-and-profiles › Config schema versioning and migration |
| CLI-294 | Environment variables reference (`HERMES_*` non-secret behavior vars) | N/A | Deliberately replaced: configuration-and-profiles › Environment is secrets-only; provider credential env names covered by model-providers. |
| CLI-295 | Configuration precedence (CLI flags > env > config.yaml > defaults), `${VAR}` substitution in config | Specified | configuration-and-profiles › Configuration layering; › Secrets are not configuration (`{ secret = "NAME" }` replaces `${VAR}`) |
| CLI-296 | TUI feature page: collapsible banner, live session switcher, LaTeX rendering, light-terminal detection, busy indicator styles, auto-resume, external prebuild | Specified | tui › Live session switcher; tui › LaTeX math rendering; tui › Terminal background detection; tui › Collapsible startup banner; tui › Auto-resume of the last TUI session |
| CLI-297 | Profiles feature page: token locks per gateway, persistent services per profile, multiplexed gateways, naming the default profile, tab completion | Specified | configuration-and-profiles › Profiles are islands; › Served profile set; platform-adapters › Credential locks across homes |
| CLI-298 | `/usage [reset [--force]]` token usage, cost, account limits; `reset` redeems banked Codex limit reset | Specified | model-providers › Account usage windows command |
| CLI-299 | `/subscription` (alias `/upgrade`) view Nous plan, change in browser | Specified | model-providers › Subscription account commands |
| CLI-300 | `/login` sign in with Nous account off-turn (paired DMs only on gateway) | Specified | model-providers › Subscription account commands |
| CLI-301 | `/topup` show balance and manage billing on portal | Specified | model-providers › Subscription account commands |
| CLI-302 | `/insights [days]` | Specified | observability › Usage insights report |
| CLI-303 | `/platforms` (alias `/gateway`) gateway/platform status in CLI | Specified | messaging-gateway › Adapter operations from chat |
| CLI-304 | `/platform <pause\|resume\|list> [name]` (gateway) operate an adapter, clear tripped breaker | Specified | messaging-gateway › Adapter operations from chat |
| CLI-305 | `/copy [number]` copy last (Nth) assistant response to clipboard | Specified | tui › Slash command flow in the TUI (`/copy`; `[number]` arg and line-mode support to add) |
| CLI-306 | `/paste` attach clipboard image | Specified | tui › Clipboard image attachments |
| CLI-307 | `/image <path>` attach local image for next prompt | Specified | cli › Image attachments |
| CLI-308 | `/update` update from chat (gateway uses file-based IPC for prompts/progress) | Specified | distribution-and-update › Update from chat |
| CLI-309 | `/version` | Specified | cli › Local transcript and session commands |
| CLI-310 | `/debug [nous\|local]` upload debug report | Specified | observability › Opt-in debug share upload |
| CLI-311 | `/quit` (alias `/exit`) `[--delete]` | Specified | cli › Exit with delete and profile archive slash forms |
| CLI-312 | `/connectors` (TUI-only) connector account list | Specified | connectors › Connectors slash command |

## Messaging, API and user surfaces

| ID | Hermes feature | Status | Prometheus requirement |
|---|---|---|---|
| SURF-001 | Platform capability comparison (voice/images/files/threads/reactions/typing/streaming per platform) | Specified | platform-adapters › Capability descriptor is the single source of truth; `prometheus gateway platforms` prints descriptors |
| SURF-002 | First-party adapter transports (Telegram Bot API poll/webhook, Discord gateway WS, Slack Socket Mode, Email IMAP poll + SMTP, Matrix /sync, Signal via signal-cli HTTP daemon SSE+JSON-RPC, WhatsApp via Baileys Node bridge) | Specified | platform-adapters › First-party adapter transports and required settings |
| SURF-003 | Telegram adapter: DMs, groups, topics, inline keyboards, streaming edits, voice in/out, media, typing | Specified | platform-adapters › First-party platforms; › Default first-party message limits (4096 utf16); › Interactive approval cards |
| SURF-004 | Telegram webhook mode (`TELEGRAM_WEBHOOK_URL`, `TELEGRAM_WEBHOOK_SECRET` secret-token header, Fly.io example) vs long polling | Specified | platform-adapters › Telegram webhook mode |
| SURF-005 | Telegram private-chat topics (Bot API 9.4 DM topics), root-DM handling, per-topic skill binding | Specified | platform-adapters › Telegram topics and per-topic skills |
| SURF-006 | Telegram multi-session DM mode `/topic` (create/list/restore/rename, auto-renamed topics, `/new` inside topic, disable/downgrade) | Specified | messaging-gateway › Multi-session DM topics |
| SURF-007 | Telegram group forum topic skill binding | Specified | messaging-gateway › Per-chat policy thread matching and skill preload |
| SURF-008 | Telegram local Bot API server for >20 MB files (`base_url`, `local_mode` file access on disk, logOut migration) | Specified | platform-adapters › Telegram local Bot API server |
| SURF-009 | Telegram network resilience: DNS-over-HTTPS fallback IPs / direct-IP failover, HTTP(S)/SOCKS proxy support | Specified | platform-adapters › Telegram network resilience; › Adapter proxy settings and environment trust |
| SURF-010 | Telegram command menu (setMyCommands priority + 100-cap) and inline command picker (search all commands) | Specified | platform-adapters › Native command menus and slash registration |
| SURF-011 | Telegram group privacy mode guidance + "observe group chatter without auto-replying", `mention_patterns`, `guest_mode` @mention bypass | Specified | messaging-gateway › Group mention gating and observe mode |
| SURF-012 | Telegram UX extras: message reactions, push-notification volume (silent sends), status messages edited in place, pin incoming user message during turn, online/offline status indicator, cold-boot pending queue | Specified | platform-adapters › Telegram presentation options |
| SURF-013 | Telegram rich rendering (MarkdownV2/entities, tables, link previews) | Specified | platform-adapters › Markup conversion per platform |
| SURF-014 | Telegram Quick setup via QR (dashboard/desktop: create bot, detect user ID, save creds, restart) | Specified | desktop-app › Telegram quick setup by QR |
| SURF-015 | Interactive model picker on Telegram/Discord (`/model` with buttons) | Specified | messaging-gateway › Interactive model picker and chat model override |
| SURF-016 | Discord adapter: guilds/DMs, threads, reactions, files, voice messages, streaming | Specified | platform-adapters › First-party platforms; › Default first-party message limits (2000 chars) |
| SURF-017 | Discord voice channels (`/voice join\|leave`, per-speaker STT, TTS playback, echo prevention, ambient + verbal acks, text-channel integration, access control) | Specified | voice-mode › Discord voice channel sessions; › Voice channel echo suppression, access control and rebinding |
| SURF-018 | Discord native slash commands for skills (auto-registered, `slash_commands: false` to disable) + slash command access control | Specified | platform-adapters › Native command menus and slash registration |
| SURF-019 | Discord threading: `auto_thread`, `free_response_auto_thread`, `no_thread_channels`, thread titles | Specified | platform-adapters › Discord auto-threading and forum channels |
| SURF-020 | Discord forum channels | Specified | platform-adapters › Discord auto-threading and forum channels |
| SURF-021 | Discord history backfill (`DISCORD_HISTORY_BACKFILL`, scrollback since last bot reply prepended) | Specified | messaging-gateway › Channel history backfill |
| SURF-022 | Discord role-based access control (allowed roles) | Specified | messaging-gateway › Role-based access grants |
| SURF-023 | Bot-to-bot handoffs (Discord tag-once collect burst; Slack `allow_bots`, `api_human_users`; Feishu bot-to-bot) | Specified | messaging-gateway › Bot-authored message admission |
| SURF-024 | Processing-status reactions (👀 start, ✅ done, ❌ error) on Discord, Slack, Feishu, DingTalk, Signal | Specified | platform-adapters › Processing lifecycle reactions |
| SURF-025 | Slack adapter (Socket Mode, mrkdwn, threads, Block Kit approvals) | Specified | platform-adapters › First-party platforms; › Interactive approval cards |
| SURF-026 | `hermes slack manifest` app-manifest generator (`--name`, slash-commands-only output, long description) | Specified | platform-adapters › Slack app manifest generator |
| SURF-027 | Slack slash commands (one per command, legacy `/hermes <sub>`, `!cmd` prefix inside threads, ephemeral replies, refresh after updates) | Specified | platform-adapters › Slack slash commands and thread command prefix |
| SURF-028 | Slack native streaming (live-typing replies), native task cards for tool progress, working-state status line (assistant thread status) | Specified | messaging-gateway › Native platform streaming and task cards |
| SURF-029 | Slack reaction triggers (`reaction_triggers`: emoji reaction on a message starts a turn) | Specified | platform-adapters › Slack reaction triggers |
| SURF-030 | Slack multi-workspace (OAuth token file, per-team tokens) | Specified | platform-adapters › Slack multiple workspaces |
| SURF-031 | Per-channel prompts (Telegram, Discord, Slack, Mattermost) | Specified | messaging-gateway › Per-chat policies (upgrade) (`system_prompt_append`) |
| SURF-032 | Per-channel skill bindings (auto-load skill in channel; Discord/Slack/Telegram) | Specified | messaging-gateway › Per-chat policy thread matching and skill preload |
| SURF-033 | Peer-agent smoke check (Slack) | Specified | platform-adapters › Peer-agent routing self-test (upgrade) |
| SURF-034 | Email adapter (IMAP poll, SMTP reply, threading, attachments, skip-attachments option, Gmail/Outlook/Proton Bridge) | Specified | platform-adapters › Email adapter behaviour; › First-party adapter transports and required settings |
| SURF-035 | Matrix adapter (rooms/DMs, threads, HTML, media, mention gating) | Specified | platform-adapters › First-party platforms (32000 utf8_bytes) |
| SURF-036 | Matrix end-to-end encryption (E2EE device keys store, cross-signing verification, decrypt errors, upgrade) | Specified | platform-adapters › Matrix end-to-end encryption |
| SURF-037 | Matrix proxy mode (E2EE on macOS host relayed into Docker VM) | Specified | platform-adapters › Adapter host forwarding mode |
| SURF-038 | Matrix project room isolation, `allowed_rooms`, auto-join invites, clock-skew drop diagnostics | Specified | platform-adapters › Matrix rooms, invites and clock skew |
| SURF-039 | Signal extras: phone-number redaction in logs, Note-to-Self single-number setup, native text styles, reply quotes, group allowlist, health monitoring | Specified | platform-adapters › Signal adapter specifics |
| SURF-040 | WhatsApp (Baileys bridge): `hermes whatsapp` QR pairing, bot mode vs self-chat mode, session persistence, re-pairing, multiple profiles | Specified | platform-adapters › WhatsApp pairing and modes |
| SURF-041 | WhatsApp native polls, clarify-as-poll, locations, quoted replies, WhatsApp-compatible Markdown, message debounce | Specified | platform-adapters › WhatsApp interactive and rich features; › Markup conversion per platform |
| SURF-042 | WhatsApp Cloud API adapter (`hermes whatsapp-cloud` setup wizard, Graph API, X-Hub-Signature-256, interactive buttons, read receipts + typing, voice notes, 24-hour window, outbound rate limit) | Specified | platform-adapters › WhatsApp Cloud API adapter |
| SURF-043 | Webhook adapter routes (`events`, `secret`/INSECURE_NO_AUTH, `prompt` dot-templates, `skills`, `toolsets`, `profile`) | Specified | platform-adapters › Webhook routes; › Webhook limits, idempotency and responses |
| SURF-044 | Webhook payload filters and script filters/transforms (`filters`, `script` under scripts/ with stdin JSON) | Specified | platform-adapters › Webhook payload filters and transform scripts |
| SURF-045 | Webhook event coalescing (`coalesce.key`, window) | Specified | platform-adapters › Webhook event coalescing |
| SURF-046 | Webhook delivery targets (`deliver`: github_comment, any platform, log; `deliver_extra`; forum topic delivery; replying to a delivery; `mirror_to_session`) | Specified | messaging-gateway › Webhook route delivery and safe default toolset |
| SURF-047 | Webhook direct delivery mode (`deliver_only`: no LLM, rendered template sent) | Specified | platform-adapters › Webhook direct delivery mode |
| SURF-048 | Webhook → event-triggered cron job (`cron_job`) | Specified | scheduled-jobs › Webhook-triggered job runs |
| SURF-049 | Dynamic webhook subscriptions CLI `hermes webhook subscribe\|list\|remove\|test` (--events, --skills, --deliver, --secret auto-gen) | Specified | cli › Webhook subscriptions |
| SURF-050 | Webhook MCP events (MCP-server events into webhook pipeline) | Specified | platform-adapters › MCP server events into the webhook pipeline |
| SURF-051 | Dashboard Webhooks page | Specified | web-dashboard › Webhooks page |
| SURF-052 | OpenAI-compatible API server: `POST /v1/chat/completions` (streaming), `GET /v1/models`, port 8642, `API_SERVER_KEY` bearer | Specified | api-server › Opt-in listener that fails closed; api-server › Chat Completions endpoint; api-server › Model listing and Open WebUI compatibility |
| SURF-053 | Responses API `POST/GET/DELETE /v1/responses`, `previous_response_id` chaining, stored history size cap | Specified | api-server › Responses endpoint and response store |
| SURF-054 | Runs API `/v1/runs` (+ `/events` SSE, `/stop`, `/approval`, `/clarify`, `/resolve-unknown`) | Specified | api-server › Runs API lifecycle; api-server › Run events stream; api-server › Run controls |
| SURF-055 | Idempotency keys on API submissions | Specified | api-server › Idempotent admission for Chat Completions and Responses; api-server › Run idempotency ledger |
| SURF-056 | Jobs API `/api/jobs` CRUD + pause/resume/run | Specified | api-server › Jobs REST API |
| SURF-057 | Sessions REST API `/api/sessions/*` (list, fork, chat/stream) | Specified | api-server › Sessions REST API |
| SURF-058 | Discovery endpoints: `/v1/capabilities`, `/api/model/options`, skills/toolsets discovery, per-request model selection, `/health`, `/health/detailed` | Specified | api-server › Discovery and readiness endpoints; api-server › Per-request model selection |
| SURF-059 | `X-Hermes-Session-Key` long-term memory scoping and system-prompt handling for API callers | Specified | api-server › Caller instructions and long-term memory scoping; api-server › API session identity and ownership |
| SURF-060 | API server security & config: multi-profile `/p/<profile>/…` routing, CORS origins, security headers, concurrent-run cap, proxy mode | Specified | api-server › Profile-bound routing and bearer authentication; api-server › Browser access, Host checks and security headers; api-server › Concurrent run cap; api-server › Gateway proxy mode |
| SURF-061 | Open WebUI / compatible frontends integration (Chat Completions vs Responses, multi-user via profiles, Docker compose) | Specified | api-server › Model listing and Open WebUI compatibility |
| SURF-062 | Bot chat & hosted room dispatch over API server (room grants, authority runs, turn boundary) | Specified | bot-mode › Hosted rooms across gateways; bot-mode › Durable room driver |
| SURF-063 | Microsoft Graph webhook listener (`msgraph_webhook`: validationToken handshake, `clientState` check, source-IP allowlist, HTTPS termination) | Specified | platform-adapters › Microsoft Graph change-notification listener |
| SURF-064 | Teams meeting pipeline (Graph subscriptions, transcript/recording fetch, summary, delivery `incoming_webhook`\|`graph`; `hermes teams-pipeline` CLI) | Specified | platform-adapters › Teams meeting summary pipeline (new) |
| SURF-065 | Microsoft Teams adapter (Bot Framework, personal/group/channel, Adaptive Card approvals, typing) | Specified | platform-adapters › Microsoft Teams adapter |
| SURF-066 | Google Chat adapter (HTTP callbacks or Pub/Sub pull, cards for clarify, working-state marker, `/setup-files` per-user OAuth for native attachments) | Specified | platform-adapters › Google Chat adapter |
| SURF-067 | Mattermost adapter (v4 REST + WS, thread reply mode, `allowed_channels`, mention behavior, home channel) | Specified | platform-adapters › Mattermost adapter |
| SURF-068 | DingTalk adapter (Stream Mode, AI Cards streaming, emoji reactions, session_webhook replies, @mention gating) | Specified | platform-adapters › DingTalk adapter |
| SURF-069 | Feishu/Lark adapter (WebSocket or webhook, scan-to-create app, card actions, markdown→post fallback, media/text batching, per-chat serialization, webhook rate limiting + anomaly tracking, per-group ACL) | Specified | platform-adapters › Feishu and Lark adapter |
| SURF-070 | Feishu document comment intelligent reply (drive comment events, 3-tier access control) | Specified | platform-adapters › Feishu document comments and meeting invitations |
| SURF-071 | Feishu meeting invitation events | Specified | platform-adapters › Feishu document comments and meeting invitations |
| SURF-072 | WeCom smart-robot adapter (WebSocket, AES-encrypted media, reply-mode streaming, DM/group policies, per-group sender allowlists) | Specified | platform-adapters › WeCom smart robot adapter |
| SURF-073 | WeCom Callback (self-built app HTTP callback, AES message crypto, multi-app routing) | Specified | platform-adapters › WeCom callback adapter |
| SURF-074 | Weixin personal WeChat (iLink long-poll, QR login in setup, context_token persistence, AES-128-ECB CDN media, token lock) | Specified | platform-adapters › Weixin personal WeChat adapter |
| SURF-075 | BlueBubbles iMessage (REST + registered webhook, tapbacks, read receipts, typing via Private API, group wake-word gating) | Specified | platform-adapters › BlueBubbles iMessage adapter |
| SURF-076 | Photon (iMessage via Spectrum gRPC stream, supervised Node sidecar, outbound attachments, inbound metadata-only) | Specified | platform-adapters › Photon iMessage adapter; › Supervised sidecar adapters |
| SURF-077 | QQ Bot (official API v2 WS, keyboards, chunked upload, voice STT) | Specified | platform-adapters › QQ Bot adapter |
| SURF-078 | Tencent Yuanbao (WS with HMAC sign token, stickers, media uploads, chat id formats) | Specified | platform-adapters › Yuanbao adapter |
| SURF-079 | LINE (webhook HMAC-SHA256, free reply token then metered Push fallback, slow-response template button) | Specified | platform-adapters › LINE adapter |
| SURF-080 | SMS via Twilio (webhook signature validation, markdown stripped, plain text) | Specified | platform-adapters › SMS adapter |
| SURF-081 | ntfy (topic subscribe via HTTP stream, publish replies, topic = single trusted channel, outgoing-only mode for cron) | Specified | platform-adapters › ntfy adapter |
| SURF-082 | IRC (stdlib, channel + DMs, nick) | Specified | platform-adapters › IRC adapter |
| SURF-083 | Raft (wake-channel bridge, content-free wake hints, agent uses Raft CLI) | Specified | platform-adapters › Raft wake-only adapter |
| SURF-084 | Buzz (Nostr community relay via buzz CLI, reply threading, mentions/channels/DMs, verified inbound media) | Specified | platform-adapters › Buzz Nostr adapter |
| SURF-085 | SimpleX Chat (simplex-chat WS daemon, opaque contact IDs, groups, attachments) | Specified | platform-adapters › SimpleX adapter |
| SURF-086 | Home Assistant (catalog plugin: event platform with filtering + `ha_list_entities`/`ha_get_state`/`ha_list_services`/`ha_call_service` tools) | Specified | platform-adapters › Home Assistant adapter and toolset; › Catalog adapter set |
| SURF-087 | A2A inbound server (Agent Card at `/.well-known/agent-card.json`, JSON-RPC 2.0 `SendMessage`/`SendStreamingMessage`/`GetTask`/`ListTasks`/`CancelTask`/`SubscribeToTask`, SSE, HMAC-signed push notifications; contextId → session) | Specified | agent-interop › A2A server enablement and binding; agent-interop › Agent Card; agent-interop › A2A JSON-RPC methods and task lifecycle; agent-interop › A2A conversation identity; agent-interop › A2A authentication, trust and rate limits; agent-interop › A2A inbound safety; agent-interop › A2A push notifications |
| SURF-088 | A2A outbound tools `a2a_discover`, `a2a_call`, `a2a_list`, `a2a_history`, `a2a_orchestrate` | Specified | agent-interop › Outbound A2A tools |
| SURF-089 | Hermes Relay connector (outbound WS, capability descriptor handshake, connector owns credentials) | Specified | messaging-gateway › Relay connector |
| SURF-090 | `hermes gateway enroll` (`--token`, `--connector-url`, `--gateway-id`, `--wake-url`) | Specified | messaging-gateway › Relay enrollment |
| SURF-091 | `hermes peer add\|list\|remove\|dm\|run\|status\|stop` (peer gateways over API server, `<peer>/<agent>`, `--idempotency-key`, `--json`, exit 0/1/2) | Specified | agent-interop › Peer gateway registry; agent-interop › Peer direct messages and remote runs |
| SURF-092 | Intentional silence tokens (`[SILENT]`, `SILENT`, `NO_REPLY`, Chinese variants) suppress delivery; person-sent message gets a notice instead | Specified | messaging-gateway › Intentional silence tokens |
| SURF-093 | Group mention gating across platforms (`require_mention`, `free_response_channels`, `mention_patterns`, `thread_require_mention`, group wake words) | Specified | messaging-gateway › Group mention gating and observe mode |
| SURF-094 | Home channel and `/sethome` command; cron/notification delivery to home | Specified | messaging-gateway › Home channel and /sethome |
| SURF-095 | `send_message` tool / `hermes send` cross-platform delivery with targets (`platform:chat[:thread]`, media) | Specified | agent-interop › send_message tool; agent-interop › One-shot send command |
| SURF-096 | Tool progress notifications (`display.tool_progress` off/new/all/verbose/log, grouping accumulate/separate, `log` audit file 5 MB×3, status phrases, mobile defaults, progress bubble cleanup, `/verbose` in messaging) | Specified | messaging-gateway › Display policy and per-platform overrides |
| SURF-097 | Message timestamps in model context | Specified | messaging-gateway › Message timestamps in model context |
| SURF-098 | Busy input: queue vs interrupt vs steer | Specified | messaging-gateway › Busy input policy from configuration only |
| SURF-099 | Clarify questions incl. multi-select on platforms (Discord/Slack/Google Chat buttons) | Specified | platform-adapters › Clarify multi-select on platforms |
| SURF-100 | Background sessions (`/background <prompt>` runs separate session, result delivered back) | Specified | messaging-gateway › Background sessions from chat |
| SURF-101 | Background process notifications | Specified | messaging-gateway › Background and internal notifications |
| SURF-102 | Service management (systemd user/system `--system`, `--run-as-user`, launchd, Windows Task Scheduler at login, `gateway install/uninstall/start/stop/restart`) | Specified | messaging-gateway › Per-OS service installation without elevation |
| SURF-103 | Gateway stack dump on SIGUSR2 and event-loop liveness watchdog (probe 30 s, 3 strikes → degraded record, exit 75) | Specified | observability › Runtime liveness watchdog and task dump |
| SURF-104 | `/platform list\|pause\|resume` and automatic per-adapter circuit breaker (no auto-resume) | Specified | messaging-gateway › Adapter operations from chat |
| SURF-105 | `gateway.trust_env` (ignore inherited HTTP proxy env) | Specified | platform-adapters › Adapter proxy settings and environment trust |
| SURF-106 | Restart notifications ("gateway restarted" to active chats after graceful restart/update) | Specified | messaging-gateway › Restart and update from chat |
| SURF-107 | Session resume across gateway restarts | Specified | messaging-gateway › Crash recovery and clean shutdown marker; › Conversations never rotate on time |
| SURF-108 | Warning/error notification opt-in suppression | Specified | messaging-gateway › Warning notification suppression |
| SURF-109 | DM pairing and `hermes pairing list\|approve\|revoke\|clear-pending` | Specified | messaging-gateway › Documented pairing flow |
| SURF-110 | Admins vs regular users | Specified | messaging-gateway › Slash command access tiers |
| SURF-111 | Platform-specific toolsets (`platform_toolsets`) | Specified | tool-system › Surface toolsets come from the session |
| SURF-112 | Persistent `/model` overrides per chat, `/sessions` in messaging | Specified | messaging-gateway › Interactive model picker and chat model override |
| SURF-113 | Inbound/outbound media, `MEDIA:` tags | Specified | platform-adapters › Inbound media limits and caching; › Outbound attachments and path safety |
| SURF-114 | Docker-backed terminal media path translation (files generated in container delivered natively) | Specified | platform-adapters › Container media path translation |
| SURF-115 | Deliverable mode: extension → native kind table (images inline, audio as voice, docs/data/archives as file uploads), artifacts on kanban completion notifications | Specified | platform-adapters › Deliverable mode; › Response-wide delivery modifiers and kanban artifacts |
| SURF-116 | Voice transcription of inbound voice notes | Specified | platform-adapters › Voice input transcription |
| SURF-117 | Gateway voice reply modes and `/voice on\|off\|tts\|status` in chats | Specified | voice-mode › Gateway per-chat voice reply command |
| SURF-118 | Streaming to platforms | Specified | messaging-gateway › Streaming to platforms |
| SURF-119 | Typing indicators | Specified | platform-adapters › Typing indicator and human delay |
| SURF-120 | Text/media batching, dedup, per-chat serialization | Specified | platform-adapters › Inbound deduplication and text batching; messaging-gateway › Single-flight session routing |
| SURF-121 | Messaging status in Desktop/dashboard (Saved vs connected vs gateway stopped, per profile/machine) | Specified | desktop-app › Messaging pane shows saved versus running state; web-dashboard › Channel setup forms |
| SURF-122 | `hermes gateway setup` interactive per-platform wizard (offers restart) | Specified | cli › Setup wizard modes |
| SURF-123 | One gateway serving all profiles (multiplexing) | Specified | messaging-gateway › Profiles served by one daemon |
| SURF-124 | `profile_routes`: route shared-bot chats to different profiles | Specified | messaging-gateway › Shared-credential profile routes |
| SURF-125 | Per-profile gateways, `gateway.standalone`, legacy migration `gateway migrate-legacy` | N/A | Prometheus has one daemon per home by design; import handled by hermes-migration › Gateway settings and pairing allowlists |
| SURF-126 | Token-conflict safety across profiles | Specified | platform-adapters › Credential locks across homes |
| SURF-127 | `hermes gateway list` / status / logs per profile; identify what's running | Specified | messaging-gateway › One gateway daemon per home (`gateway status` per profile/adapter); cli › Log viewer |
| SURF-128 | Keep host awake guidance (caffeinate / systemd-inhibit) | N/A | Documentation-only OS guidance, no Hermes behavior |
| SURF-129 | Kanban across multiple gateways (single dispatcher, profile-owned delivery) | Specified | kanban › Single dispatcher per home; › Notifications through the owning profile |
| SURF-130 | Subscription proxy `hermes proxy start\|status\|providers` (Nous Portal/OAuth sub as OpenAI endpoint, port 8645, allowed paths chat/completions/embeddings/models, LAN exposure, rate limits) | Specified | api-server › Subscription proxy |
| SURF-131 | `hermes dashboard` (port 9119, `--host`, `--no-open`, `--stop`, `--status`, profile-scoped server) | Specified | web-dashboard › Dashboard command and listener defaults (Prometheus default 9120) |
| SURF-132 | Dashboard gated auth providers: Nous Portal OAuth (default), username/password (scrypt hash), deployment session token, self-hosted OIDC, public URL override; `hermes dashboard register` | Specified | web-dashboard › Additional dashboard sign-in providers; web-dashboard › Registering a dashboard with the account portal |
| SURF-133 | Dashboard core pages (Status, Chat, Config, API Keys/Env, Sessions, Logs, Analytics, Cron, Profiles, Skills, MCP, Pairing, Models, Plugins, System) | Specified | web-dashboard › Built-in pages; › Secrets are write-only; › Config editing is schema-driven; › Cost and usage view |
| SURF-134 | Dashboard Channels page (per-platform setup forms incl. WhatsApp QR pairing) | Specified | web-dashboard › Channel setup forms |
| SURF-135 | Dashboard Files page | Specified | web-dashboard › Files page |
| SURF-136 | Dashboard Docs page (embedded docs) | Specified | web-dashboard › Embedded documentation |
| SURF-137 | Dashboard Profile Builder page | Specified | web-dashboard › Profile builder |
| SURF-138 | `/reload` slash command (reload config/.env into running gateway) | Specified | configuration-and-profiles › Live reload with apply classes; messaging-gateway (`gateway reload`) |
| SURF-139 | Dashboard REST API (`/api/status`, `/api/config[/schema\|/defaults]`, `/api/env`, sessions search/messages, analytics usage, cron, skills toggle, toolsets, admin endpoints, bulk maintenance) | Specified | web-dashboard › REST endpoint inventory (new); web-dashboard › Profile-addressed routes |
| SURF-140 | Dashboard themes (palette, typography, layout variants, theme assets, component chrome, customCSS, built-in themes, theme YAML) | Specified | web-dashboard › Dashboard themes |
| SURF-141 | Dashboard plugins (manifest, SDK on `window`, shell slots, `tab.override`, page-scoped slots, `tab.hidden`, backend API routes, per-plugin CSS, discovery & reload) | Specified | web-dashboard › Dashboard plugin contributions |
| SURF-142 | Dashboard chat | Specified | web-dashboard › Structured chat over the client protocol |
| SURF-143 | `hermes webapp` (Desktop workspace in a browser, one-use link, `--no-open`, `--status`, `--sign-out` all tabs, open from phone/other computer, terminal in webapp) | Specified | web-dashboard › Desktop workspace in the browser; web-dashboard › Webapp sign-out and insecure-context notice; web-dashboard › Webapp file staging and terminals |
| SURF-144 | `hermes console` safe curated command REPL (not a shell) | Specified | cli › Safe command console |
| SURF-145 | `hermes connectors list\|accounts\|connect\|rename\|disconnect` (hosted app connectors like Gmail via tool gateway, multi-account aliases, `--reconnect`) | Specified | connectors › Connector account management CLI; › Multiple accounts per connector; › Reconnect and rename semantics |
| SURF-146 | `hermes desktop\|gui` launcher (`--cwd`, build/rebuild, `--skip-build`, local-models UI flag, macOS TCC signing identity, Linux AppArmor profile, close-preview escape hatch) | Specified | desktop-app › Launching the desktop from the CLI |
| SURF-147 | `hermes serve` headless backend for desktop & remote backends | Specified | messaging-gateway › Remote client attachment |
| SURF-148 | Desktop chat UX: streaming tool activity with image expand, drag-drop attach, background drafts, unsent-draft restore, composer history & queue editing, task progress, timeline rail, reading-position memory, find in page, restore image draft | Specified | desktop-app › Chat transcript rendering; desktop-app › Drafts are durable and never sent for the user; desktop-app › Composer history and queue editing |
| SURF-149 | Desktop status bar (per-session YOLO toggle, context meter with category breakdown, cache hit rate, tok/s, customizable items) | Specified | desktop-app › Desktop status bar |
| SURF-150 | Desktop composer model picker (sticky per device, favorites, per-model effort/fast presets, reasoning pill) and Settings → Model/Subagents | Specified | desktop-app › Composer model picker; desktop-app › Subagent route settings are drafts until applied |
| SURF-151 | Desktop file browser pane | Specified | desktop-app › File browser pane |
| SURF-152 | Desktop artifacts gallery & preview rail (web pages/files side-by-side), in-app browser with Annotate comment mode | Specified | desktop-app › Artifacts gallery and suggestions; desktop-app › Preview rail and in-app browser; desktop-app › Annotate mode in the in-app browser |
| SURF-153 | Desktop windows/tabs/panes, pop-out windows, Interface mode Simple/Advanced, layouts | Specified | desktop-app › Windows, tabs and panes; desktop-app › Interface modes and layouts |
| SURF-154 | Desktop embedded terminal (multiple persistent shells, Add to chat) | Specified | desktop-app › Embedded terminals are daemon-owned PTYs |
| SURF-155 | Desktop live subagents frame (roster, Steer/Stop) | Specified | desktop-app › Live subagents frame |
| SURF-156 | Desktop Git review pane (uncommitted/branch/last-turn diffs, stage/revert, commit) and worktrees | Specified | desktop-app › Git review pane (worktree lanes defer to git-worktrees) |
| SURF-157 | Desktop Memory Graph / Star Map (`/journey`, `/learning`, `/memory-graph`) | Specified | desktop-app › Memory graph |
| SURF-158 | Desktop Quick Entry global hotkey | Specified | desktop-app › Global hotkey quick-ask (upgrade) |
| SURF-159 | Desktop HUD mode (always-on-top bar, tap-to-summon, snap to pointer, position as context) | Specified | desktop-app › HUD mode |
| SURF-160 | Desktop minimize/close to tray | Specified | desktop-app › Closing windows does not stop the agent |
| SURF-161 | Desktop management panes (Skills install/toggle, Cron all-profiles, Profiles, Messaging, Agents, Command Center) | Specified | desktop-app › Management panes |
| SURF-162 | Desktop repository discovery & Projects sidebar (`desktop.repo_scan_enabled/roots/exclude_paths`) | Specified | desktop-app › Repository discovery for the Projects sidebar |
| SURF-163 | Desktop fonts (`desktop.font_family`, `terminal.font_family`), rebindable shortcuts, command palette, profile export/import | Specified | desktop-app › Fonts, shortcuts, palette and profile files; desktop-app › Machine-wide UI preferences |
| SURF-164 | Multi-connection desktop: gateway registry of local + remote backends (URL or SSH), session groups, switching/scoping, update every instance | Specified | desktop-app › Connection registry; › Remote connections are authenticated and encrypted; › SSH connections reuse the local ticket model; › Gateway switching and scoping; › Update every connected instance; › Local endpoint validation (revised); web-dashboard › Paired device credentials for remote clients |
| SURF-165 | Agents across gateways (bots on several machines in one roster) | Specified | agent-interop › Union roster across gateways |
| SURF-166 | Bot Mode roster: profiles as named bots with role, model, avatar (geometric/uploaded/AI/pet), sections, "Create on" machine | Specified | bot-mode › A bot is a profile with backend metadata; bot-mode › Creating, duplicating and deleting bots; bot-mode › Avatars; bot-mode › Roster presentation, hiding and sections |
| SURF-167 | Bot voices | Specified | bot-mode › Bot voices |
| SURF-168 | Bot Routines (recurring tasks backed by cron) | Specified | bot-mode › Bot routines |
| SURF-169 | Bot groups & group chats; bot-to-bot messaging (typed delivery failure reasons, safe retry) | Specified | bot-mode › Group chats; bot-mode › Room stop directives and held members; bot-mode › message_agent tool; bot-mode › Typed delivery failures and safe retry |
| SURF-170 | Cross-machine bot messaging (Desktop relay, `hermes peer`), transferring hosted room authority | Specified | agent-interop › Desktop relay for cross-connection bot messages; agent-interop › Peer direct messages and remote runs; bot-mode › Room authority transfer |
| SURF-171 | Bot Mode off switch and CLI parity | Specified | bot-mode › Bot Mode off switch and CLI parity |
| SURF-172 | Bot Screen: per-bot Xfce desktop on headless host streamed into Desktop (RFB), take over / hand back, browser sessions survive handoff, screen inside sandbox backend, CLI | Specified | execution-environments › Bot screen (+ Bot screen takeover lease) |
| SURF-173 | Desktop pets / pet overlay | Specified | pets › Desktop pet and pop-out overlay |
| SURF-174 | Desktop wake indicator & voice controls (dictation mic, read replies aloud, wake ear) | Specified | voice-mode › Desktop voice controls |
| SURF-175 | TUI launch, gateway attach, reconnect | Specified | tui › TUI launch selection; › TUI attaches to the gateway daemon as a client; › Event sequencing and reconnect replay |
| SURF-176 | TUI keybindings & status line | Specified | tui › Configurable key bindings; › Status line and accessibility |
| SURF-177 | TUI live session switcher | Specified | tui › Live session switcher |
| SURF-178 | TUI LaTeX math rendering | Specified | tui › LaTeX math rendering |
| SURF-179 | TUI light-terminal detection, busy indicator styles, collapsible banner sections, auto-resume last session | Specified | tui › Terminal background detection; tui › Busy indicator styles; tui › Collapsible startup banner; tui › Auto-resume of the last TUI session |
| SURF-180 | CLI voice mode (`/voice on\|off\|tts\|status`, push-to-talk, silence detection, end chat by voice, hallucination filter) | Specified | voice-mode › Voice mode toggle and commands; › Two-stage silence detection; › Spoken and typed stop phrases; › Transcription hallucination filter |
| SURF-181 | Live streaming STT, streaming TTS, barge-in | Specified | voice-mode › Live streaming transcription; › Sentence-streamed spoken replies; › Barge-in |
| SURF-182 | Desktop voice: client-direct remote voice, GPT-Live full-duplex mode delegating to agent, voice chat model | Specified | voice-mode › Client-direct voice for remote daemons; › Realtime full-duplex voice chat mode (upgrade); › Voice turn model and reasoning |
| SURF-183 | STT/TTS provider selection & long-recording upload limits | Specified | voice-mode › Long recordings and provider upload limits (provider selection itself: web-and-media-tools) |
| SURF-184 | Wake word "Hey Hermes" (engines sherpa/openWakeWord/Porcupine, custom phrase, false-trigger reduction, surfaces CLI/TUI/GUI, per-profile wake on desktop, remote client capture) | Specified | voice-mode › Wake word listener; › Wake engines and false-trigger control; › Single wake listener ownership across surfaces; › Wake capture location for remote clients; › Wake word profile routing; › Wake and voice diagnostics |
| SURF-185 | ACP server for editors (VS Code, Zed, JetBrains, Buzz Desktop), sessions, approvals, editor MCP servers | Specified | acp-adapter › Launch entry point; › New session on the daemon; › Editor MCP servers belong to the daemon session; › Approvals and clarify bridge |
| SURF-186 | ACP session-scoped edit auto-approval | Specified | acp-adapter › Edit-approval modes |
| SURF-187 | ACP optional browser tools and Buzz channels relay bridge | Specified | acp-adapter › Optional browser tools for ACP sessions; acp-adapter › Unattended ACP clients and relay bridges |
| SURF-188 | Docker image run modes (gateway, dashboard, webapp, interactive CLI), persistent volumes, state.db FS requirements, immutable install tree, exec drops to `hermes` user, PID-1 zombie reaping | Specified | distribution-and-update › Container image run modes; Container filesystem, init and health |
| SURF-189 | Docker multi-profile in one container, per-profile gateway supervision, reaching several profiles from outside | Specified | distribution-and-update › Multiple profiles in one container |
| SURF-190 | Docker Linux desktop audio bridge (PulseAudio for voice in container), local inference connectivity (host.docker.internal) | Specified | distribution-and-update › Container audio bridge and host services |
| SURF-191 | Native Windows install (install.ps1, MSIX/App Installer, Microsoft Store, dependency bootstrap) | Specified | distribution-and-update › Desktop packages and signing |
| SURF-192 | Windows feature matrix | Specified | web-dashboard › Honest platform support matrix |
| SURF-193 | Windows shell execution (Git Bash detection), UTF-8 console, Ctrl+Enter newline, external editor (`Ctrl-X Ctrl-E`, `/edit`), PATH after install | Specified | execution-environments › Windows local shell (shell + UTF-8 console only; Ctrl+Enter newline / external editor / PATH belong to cli) |

## Agent, providers, memory, skills, automation

| ID | Hermes feature | Status | Prometheus requirement |
|---|---|---|---|
| AGT-001 | Provider registry with canonical ids + aliases | Specified | model-providers › Typed profile-scoped provider registry; › Provider descriptor contract |
| AGT-002 | Shipped first-class provider set (which providers exist out of the box) | Specified | model-providers › Built-in provider catalog |
| AGT-003 | Nous Portal (OAuth device code, subscription, tool billing) | Specified | model-providers › Nous Portal subscription sign-in |
| AGT-004 | `hermes portal` / `hermes setup --portal` one-step Portal onboarding | Specified | cli › Setup wizard modes |
| AGT-005 | OpenAI Codex (ChatGPT plan) device-code + browser PKCE on :1455 | Specified | model-providers › OpenAI Codex ChatGPT plan sign-in; › Device-code and browser PKCE sign-in flows |
| AGT-006 | Anthropic native (API key, Claude Max OAuth, setup-token, Claude Code credential reuse) | Specified | model-providers › Anthropic Claude Max sign-in; › Borrowed CLI logins |
| AGT-007 | OpenRouter (API key + OAuth PKCE key minting, alias `or`) | Specified | model-providers › OpenRouter OAuth key minting; › Typed profile-scoped provider registry (alias scenario) |
| AGT-008 | Ramp Router (`router`; aliases ramp-router, ramp, router.com; Responses-native; live account catalog) | Specified | model-providers › Built-in provider catalog |
| AGT-009 | Fireworks AI (`fireworks`; fireworks-ai, fw; slash-form `accounts/fireworks/models/...` ids) | Specified | model-providers › Built-in provider catalog |
| AGT-010 | NovitaAI (`novita`) | Specified | model-providers › Built-in provider catalog |
| AGT-011 | Vercel AI Gateway (`ai-gateway`) | Specified | model-providers › Built-in provider catalog |
| AGT-012 | z.ai / GLM (`zai`) with endpoint auto-detection | Specified | model-providers › Endpoint auto-detection for multi-region key providers (+ Built-in provider catalog) |
| AGT-013 | Kimi / Moonshot (`kimi-coding`, `kimi-coding-cn`) with endpoint auto-detection | Specified | model-providers › Endpoint auto-detection for multi-region key providers (+ Built-in provider catalog) |
| AGT-014 | Arcee AI (`arcee`) | Specified | model-providers › Built-in provider catalog |
| AGT-015 | GMI Cloud (`gmi`) | Specified | model-providers › Built-in provider catalog |
| AGT-016 | Nebius Token Factory (`nebius-token-factory`) | Specified | model-providers › Built-in provider catalog |
| AGT-017 | Actual Computer (`actual`; hosted relay or keyless loopback daemon) | Specified | model-providers › Built-in provider catalog |
| AGT-018 | MiniMax API key (`minimax`, `minimax-cn`; Anthropic-Messages endpoint) | Specified | model-providers › Built-in provider catalog |
| AGT-019 | MiniMax OAuth (`minimax-oauth`) | Specified | model-providers › Qwen Portal and MiniMax portal sign-in |
| AGT-020 | xAI API key (`xai`; Responses API + prompt caching) | Specified | model-providers › Built-in provider catalog |
| AGT-021 | xAI Grok OAuth (SuperGrok / Premium+) | Specified | model-providers › xAI SuperGrok sign-in |
| AGT-022 | Qwen Cloud / DashScope (`alibaba`, `alibaba-cn`) | Specified | model-providers › Built-in provider catalog |
| AGT-023 | Alibaba Coding Plan (`alibaba-coding-plan`, `-cn`) with key fallback to DASHSCOPE_API_KEY | Specified | model-providers › Built-in provider catalog |
| AGT-024 | Alibaba Token Plan (`alibaba-token-plan`, `-cn`) | Specified | model-providers › Built-in provider catalog |
| AGT-025 | Qwen OAuth portal (`qwen-oauth`) | Specified | model-providers › Qwen Portal and MiniMax portal sign-in |
| AGT-026 | Kilo Code (`kilocode`) | Specified | model-providers › Built-in provider catalog |
| AGT-027 | Xiaomi MiMo (`xiaomi`) | Specified | model-providers › Built-in provider catalog |
| AGT-028 | Tencent TokenHub (`tencent-tokenhub`) and TokenPlan (`tencent-tokenplan`, Anthropic Messages) | Specified | model-providers › Built-in provider catalog |
| AGT-029 | OpenCode Zen / Go (`opencode-zen`, `opencode-go`) incl. per-conversation `x-opencode-session` header, relay healing and per-model wire routing | Specified | model-providers › Session affinity headers and per-model wire routing (+ Built-in provider catalog) |
| AGT-030 | CommandCode (`commandcode`, `commandcode-anthropic`) | Specified | model-providers › Built-in provider catalog |
| AGT-031 | DeepSeek (`deepseek`) | Specified | model-providers › Built-in provider catalog |
| AGT-032 | Hugging Face Inference Providers (`huggingface`, hf) | Specified | model-providers › Built-in provider catalog |
| AGT-033 | Google Gemini API key (`gemini`) | Specified | model-providers › Gemini native and AWS Bedrock providers (+ Built-in provider catalog) |
| AGT-034 | Google Vertex AI (`vertex`; service-account JSON or ADC) | Specified | model-providers › Minted short-lived credentials |
| AGT-035 | OpenAI API direct (`openai-api`, optional base URL) | Specified | model-providers › Built-in provider catalog |
| AGT-036 | Azure AI Foundry (`azure-foundry`; OpenAI-style + Anthropic-style endpoints; Entra ID keyless RBAC; model discovery) | Specified | model-providers › Azure AI Foundry provider |
| AGT-037 | AWS Bedrock (`bedrock`; AWS credential chain, Converse API, region, inference profiles) | Specified | model-providers › Gemini native and AWS Bedrock providers |
| AGT-038 | NVIDIA NIM cloud (`nvidia`) and local NIM | Specified | model-providers › Built-in provider catalog |
| AGT-039 | Ollama Cloud (`ollama-cloud`; OAuth + API key) | Specified | model-providers › Built-in provider catalog |
| AGT-040 | StepFun (`stepfun`) | Specified | model-providers › Built-in provider catalog |
| AGT-041 | Meta Model API (`meta-ai`; Muse Spark; contributor-tier confirmation) | Specified | model-providers › Data-use confirmation for training-flagged models (+ Built-in provider catalog) |
| AGT-042 | DeepInfra (`deepinfra`) | Specified | model-providers › Built-in provider catalog |
| AGT-043 | Upstage Solar (`upstage`) | Specified | model-providers › Built-in provider catalog |
| AGT-044 | Solstice (per-user-quota OAuth, pre-release plugin) | Specified | model-providers › Hermes model-provider plugins hosted by the Python bridge |
| AGT-045 | GitHub Copilot (labs plugin provider) | Specified | model-providers › Hermes model-provider plugins hosted by the Python bridge |
| AGT-046 | LM Studio (`lmstudio`; optional LM_API_KEY; base-URL normalisation) | Specified | model-providers › Local server detection and context probes; › Local endpoints never fall back to remote providers (tightened) |
| AGT-047 | Ollama local (zero-config, auto model discovery, num_ctx) | Specified | model-providers › Local server detection and context probes; › Local endpoints never fall back to remote providers (tightened) |
| AGT-048 | vLLM / SGLang / llama.cpp llama-server endpoints | Specified | model-providers › Local server detection and context probes |
| AGT-049 | LiteLLM proxy / ClawRouter / other OpenAI-compatible (Together, Groq, Perplexity, Cerebras, Mistral, LocalAI, Jan) | Specified | model-providers › Typed profile-scoped provider registry (`[providers.<id>]` custom blocks) + › Credentials are bound to their authoritative hosts |
| AGT-050 | Custom endpoint + named custom providers (`providers:` map, per-model context_length) | Specified | model-providers › Typed profile-scoped provider registry; › Model metadata catalog (`[models."<p>/<m>"]` overrides) |
| AGT-051 | Command-minted credentials (`key_cmd`) | Specified | model-providers › Minted short-lived credentials |
| AGT-052 | WSL2 networking for Windows-hosted local model servers | Specified | model-providers › WSL2 local endpoint diagnostics |
| AGT-053 | Context length detection chain (config pin, provider per-model, persistent cache, /models, Anthropic, OpenRouter, Nous suffix match, models.dev, family defaults) | Specified | model-providers › Context length detection chain |
| AGT-054 | Plan notes ("what your subscription pays for") | Specified | model-providers › Subscription and OAuth sign-in registry (plan_notes, caveats from data) |
| AGT-055 | `hermes model` interactive picker + live catalogs | Specified | model-providers › prometheus model command |
| AGT-056 | `/model` switching (provider/model, session vs global) | Specified | model-providers › /model switch command |
| AGT-057 | Per-model reasoning effort overrides and custom tier names (`agent.reasoning_overrides`) | Specified | model-providers › Reasoning effort defaults, overrides and display |
| AGT-058 | `hermes auth` add/list/remove | Specified | model-providers › Credential management commands |
| AGT-059 | `hermes auth priority`, `auth reset [target]`, `auth refresh`, `auth add --priority`, interactive `hermes auth` wizard | Specified | model-providers › Credential pool maintenance commands |
| AGT-060 | Device-code flow + browser PKCE flows (shared) | Specified | model-providers › Device-code and browser PKCE sign-in flows |
| AGT-061 | OAuth over SSH (tunnel hints, jump box) | Specified | model-providers › Device-code and browser PKCE sign-in flows (Remote machine guidance scenario) |
| AGT-062 | Plugin OAuth PKCE providers | Specified | model-providers › Plugin OAuth and external-process providers |
| AGT-063 | External-process (ACP) providers, e.g. Claude Code; executable lookup for GUI/service PATH | Specified | model-providers › External agent CLI executable resolution |
| AGT-064 | OAuth grant hygiene on cloned profiles (strip cloned single-use grants, heal forked grants) | Specified | model-providers › Subscription tokens live only in the secrets store (profiles independent); configuration-and-profiles › Profiles are islands; configuration-and-profiles › Profile creation options |
| AGT-065 | Token refresh, dead-token quarantine, re-auth messaging | Specified | model-providers › Token refresh and dead-token quarantine |
| AGT-066 | Credential pool rotation strategies (fill_first, round_robin, random, least_used) | Specified | model-providers › Credential pool selection strategies |
| AGT-067 | Pool exhaustion cooldowns (401/402/429, reset hints) | Specified | model-providers › Exhausted credential cooldowns |
| AGT-068 | Pool auto-discovery incl. numbered siblings (`OPENROUTER_API_KEY_2`, `_3`…) and auto-prune of removed sources | Specified | model-providers › Credential pool seeding, custom endpoint pools and child sharing |
| AGT-069 | Custom endpoint pools and pool sharing with subagents | Specified | model-providers › Credential pool seeding, custom endpoint pools and child sharing |
| AGT-070 | Primary fallback chain (`fallback_providers`), triggers, custom-endpoint entries | Specified | model-providers › Fallback chain configuration; › In-turn fallback activation and turn-scoped restore |
| AGT-071 | Auxiliary task routing + auto-detection chain + per-task fallback chain + capacity-error fallback | Specified | model-providers › Auxiliary task routing; › Auxiliary auto route never guesses another account |
| AGT-072 | Auxiliary concurrency limit and `extra_body` per aux task | Specified | model-providers › Auxiliary concurrency and request overrides |
| AGT-073 | Fallback for delegation and cron (pinned owners) | Specified | model-providers › Scoped fallback chains for pinned owners |
| AGT-074 | OpenRouter provider routing (`provider_routing`: sort, only, ignore, order, require_parameters, data_collection; per-model `models:` pins; `:nitro`/`:floor` suffixes) | Specified | model-providers › OpenRouter provider routing preferences |
| AGT-075 | OpenRouter Pareto Code router (`openrouter.min_coding_score`, default 0.65) | Specified | model-providers › OpenRouter provider routing preferences |
| AGT-076 | Mixture of Agents virtual provider `moa` with named presets (references + aggregator, temperatures, enabled flag, no recursion) | Specified | mixture-of-agents › MoA virtual provider and presets; › MoA call procedure; › Cache-safe guidance injection; › Strict-alternation recovery; › Reference failures are non-fatal; › MoA billing and accounting; › MoA as a fallback entry; › Preset validation |
| AGT-077 | MoA cadence `fanout` (user_turn default, per_iteration, every_n:N), privacy_filter (display/full), per-slot reasoning_effort, save_traces | Specified | mixture-of-agents › Advisor cadence; › Privacy filter for advisor outputs; › Preset validation (per-slot reasoning_effort); › MoA traces |
| AGT-078 | `/moa <prompt>` one-shot and `hermes moa list\|configure\|delete` | Specified | mixture-of-agents › One-shot /moa command; › Preset management commands |
| AGT-079 | Codex app-server runtime (`model.openai_runtime: codex_app_server`, `/codex-runtime on\|off\|auto`, `model.codex_bin`) | Specified | model-providers › Codex app-server runtime |
| AGT-080 | Codex runtime migration (MCP servers → ~/.codex/config.toml managed section, native Codex plugin migration, Hermes tools exposed back as an MCP server) | Specified | model-providers › Codex runtime migration |
| AGT-081 | Subscription proxy (`hermes proxy start\|status\|providers`; local OpenAI-compatible endpoint backed by Nous/xAI subscription) | Specified | api-server › Subscription proxy |
| AGT-082 | Prompt caching across providers | Specified | model-providers › Prompt caching and stable prefix across route changes |
| AGT-083 | Per-call cost accounting / usage | Specified | model-providers › Per-call cost accounting |
| AGT-084 | Managed local models (llama.cpp engine install, hardware-aware model builds, memory management, `local_runtime` config, detect_ports, downloads with pause/resume) | Specified | local-models › (all 13 requirements; core: Hash-pinned engine installation, Hardware-priced model catalog, Resumable, verifiable downloads, Managed server lifecycle, Context window and memory placement) |
| AGT-085 | Local LLM on Mac (llama.cpp vs MLX/omlx) and Nemotron free guide | Specified | Usage of custom/local endpoints: model-providers › Typed profile-scoped provider registry; timeouts covered by › Error classification and same-route retries |
| AGT-086 | Built-in memory (MEMORY.md agent notes + USER.md profile, two targets) | Specified | memory › Built-in store of typed entries; › Core tier character budgets |
| AGT-087 | Frozen memory snapshot in system prompt; refresh at session boundary | Specified | memory › Frozen system-prompt snapshot; › Snapshot refresh only at compression or new session |
| AGT-088 | `memory` tool actions (add/replace/remove, substring matching) | Specified | memory › Memory tool surface; › Entry matching for patch and remove (Prometheus renames replace→patch) |
| AGT-089 | Capacity limits and "memory full" behaviour | Specified | memory › Core tier character budgets; › Atomic batches judged on the final state |
| AGT-090 | Duplicate prevention on memory add | Specified | memory › Exact duplicate add is a no-op |
| AGT-091 | Memory security scanning | Specified | memory › Threat scanning of memory content |
| AGT-092 | session_search tool | Specified | session-storage › Full-text session search |
| AGT-093 | Learning Journey (`/journey`, `hermes journey [--play] list\|delete\|edit`, desktop star map) | Specified | memory › Learning journey timeline; Journey pruning and editing |
| AGT-094 | Memory enable flags (memory_enabled, user_profile_enabled) | Specified | memory › Store enable flags and tool availability |
| AGT-095 | External recall size / prefetch spill (`memory.prefetch_spill_enabled`, ceiling 10× output spill) | Specified | memory › External provider recall budget and spill |
| AGT-096 | Memory write approval (`memory.write_approval`) | Specified | memory › Write approval and pending changes |
| AGT-097 | Background review notifications (`display.memory_notifications`) | Specified | memory › Per-turn memory notifications |
| AGT-098 | Background review on a cheaper model, enabled switch, `max_input_tokens` budget (75% window cap 600k), `extra_tools`, `defer` until local GPU idle | Specified | memory › Background review controls |
| AGT-099 | Skill write approval (`skills.write_approval`) | Specified | skills › Skill mutation safety and fail-closed scanning; memory/skills write_approval keys |
| AGT-100 | Memory CLI (`hermes memory setup\|status\|off`) | Specified | memory › Memory CLI and editing; memory › Provider setup writes secrets and settings to the right stores |
| AGT-101 | External memory provider contract (prefetch, sync, mirror writes, provider tools, secret scrub) | Specified | memory › External memory provider contract; › Provider egress redaction and write mirroring; › One external provider, isolated and bounded |
| AGT-102 | Honcho memory provider (dialectic, peers, observation modes, gateway identity mapping) | Specified | python-bridge › Plugin CLI commands through the bridge (+ plugin-system › Plugin CLI subcommands) |
| AGT-103 | OpenViking memory provider | Specified | memory › External memory provider contract via python-bridge › Scope of hosted code |
| AGT-104 | Mem0 memory provider | Specified | memory › External memory provider contract (mem0 scenario) |
| AGT-105 | Hindsight memory provider | Specified | memory › External memory provider contract via python bridge |
| AGT-106 | Holographic memory provider (local HRR) | Specified | memory › External memory provider contract via python bridge |
| AGT-107 | RetainDB memory provider (in-tree until 2026-10-15) | Specified | memory › External memory provider contract via python bridge |
| AGT-108 | ByteRover memory provider (`brv` CLI; pre-compression extraction) | Specified | memory › External memory provider contract; › Pre-compress checkpoint |
| AGT-109 | Supermemory memory provider | Specified | memory › External memory provider contract via python bridge |
| AGT-110 | Memori memory provider | Specified | memory › External memory provider contract via python bridge |
| AGT-111 | Auto-install catalog plugin when a configured memory provider leaves core | N/A | Prometheus ships no bundled memory providers, so there is nothing to migrate out of core; hermes-migration handles `memory.provider` mapping on import. |
| AGT-112 | Memory provider profile isolation | Specified | memory › Profile-bound providers |
| AGT-113 | Pluggable context engines (`context.engine`, bundled + user dirs) | Specified | context-compression › Context engine selection; memory › Context engine selection |
| AGT-114 | Context files: AGENTS.md chain, .hermes.md/HERMES.md, CLAUDE.md, .cursorrules + .cursor/rules | Specified | agent-turn-loop › Project context discovery |
| AGT-115 | Progressive subdirectory AGENTS.md discovery | Specified | agent-turn-loop › Subdirectory hints ride tool results |
| AGT-116 | Context-file prompt-injection protection and size limits | Specified | agent-turn-loop › Context file scanning and size cap |
| AGT-117 | Context references (`@file`, `@folder`, `@diff`, `@url`, line ranges, tab completion, sensitive-path blocking) | Specified | agent-turn-loop › Context references in user input; Context reference safety and completion |
| AGT-118 | SOUL.md identity file | Specified | agent-turn-loop › Identity file independent of context files |
| AGT-119 | `/personality` overlays: 14 built-ins (helpful, concise, technical, creative, teacher, kawaii, catgirl, pirate, shakespeare, surfer, noir, uwu, philosopher, hype) + custom `agent.personalities`, reset | Specified | agent-turn-loop › Personality overlays |
| AGT-120 | Compression config (threshold 0.50, threshold_tokens, target_ratio, tail_mode lean, protect_first_n/last_n, in_place) | Specified | context-compression › Compression configuration schema; › Protected head; › Token-budgeted tail; › Compaction is always in place |
| AGT-121 | Gateway session hygiene (hard message limit, timeouts, turn hold, failure cooldown) | Specified | context-compression › Gateway session hygiene |
| AGT-122 | Manual `/compress` | Specified | context-compression › Manual compaction command |
| AGT-123 | Prepared-ahead summaries (`prepare_ahead`) | Specified | context-compression › Prepared-ahead summaries |
| AGT-124 | Proactive no-LLM tool-result prune (`proactive_prune_*`) | Specified | context-compression › Opt-in proactive prune; › Deterministic recoverable prune |
| AGT-125 | Idle compaction (`compression.idle_compact_after_seconds`) | Specified | context-compression › Idle compaction |
| AGT-126 | Compression progress notices to chat platforms (`compression.progress_notices`) and context pressure indicator | Specified | context-compression › Compaction progress notices and context pressure gauge |
| AGT-127 | Per-model compression threshold overrides; Codex threshold autoraise | Specified | context-compression › Per-model threshold overrides |
| AGT-128 | Native Responses server-side compaction (`compression.codex_responses_native`) | Specified | context-compression › Server-side Responses compaction (opt-in) |
| AGT-129 | Compression summary model via auxiliary + summary failure handling | Specified | context-compression › Summary failure handling; model-providers › Auxiliary task routing |
| AGT-130 | `/goal` standing goal with judge loop (done/continue/blocked/wait), 20-turn budget, pause/resume/clear | Specified | session-automation › Goal command surface; Goal judge loop; Goal turn budget and blocked verdict; Persisted per-session automation state |
| AGT-131 | Goal completion contracts (`/goal draft`, `/goal show`, inline contract) and `/subgoal` | Specified | session-automation › Completion contracts; Subgoals |
| AGT-132 | Goal quality gates (`/goal gate add\|list\|remove\|clear`; run before judge; 3 retries, 5-min timeout; admin-only add on gateway) | Specified | session-automation › Quality gates |
| AGT-133 | Goal wait barriers (judge wait_on_session/pid/seconds; `/goal wait <pid>`, `/goal unwait`) | Specified | session-automation › Goal wait barriers |
| AGT-134 | `/loop [interval] <prompt> [--times N] [--until cond]` (fixed or self-paced, LOOP_COMPLETE, pause/resume/stop, `/proactive` alias) | Specified | session-automation › Loop command and cadence; Loop stop conditions; Automation status on every surface |
| AGT-135 | `/heartbeat every <interval> <prompt>` (min 60 s, idle-only, missed ticks coalesce, `/hb`, gateway recovery, silence markers) | Specified | session-automation › Session heartbeat; Heartbeat ownership, recovery and silence on the gateway |
| AGT-136 | `delegate_task` single and batch | Specified | subagent-delegation › delegate_task tool contract; › Task validation |
| AGT-137 | Structured output (`output_schema`) | Specified | subagent-delegation › output_schema validation never discards results |
| AGT-138 | Forwarding images to a subagent | Specified | subagent-delegation › Task validation (per-task image limit 8) |
| AGT-139 | Background delegation, durable completions, independent completions (`delegation.independent_completions`) | Specified | subagent-delegation › Independent completions |
| AGT-140 | Child background-process notifications and handing a process to the parent | Specified | subagent-delegation › Child processes are reaped or handed off |
| AGT-141 | Child model override (`delegation.model/provider/base_url/api_key/api_mode`, `request_overrides`) | Specified | subagent-delegation › Child request overrides and direct endpoints |
| AGT-142 | Delegation strategies (`delegation.backend`: model_map, profile_routes via advanced-delegation plugin; `hermes delegation config`, `/delegation`) | Specified | subagent-delegation › Delegation routers fail closed; › Profile routes run the child inside the target ProfileScope |
| AGT-143 | `/review` command (review subagent with review model) | Specified | subagent-delegation › Review command |
| AGT-144 | Inherited tool access, max iterations, child timeout | Specified | subagent-delegation › Child capability grant can only narrow; › Delegation configuration defaults; › Stall detection instead of wall-clock timeouts |
| AGT-145 | Failure visibility and stall detection | Specified | subagent-delegation › Stall detection instead of wall-clock timeouts; › Structured result contract |
| AGT-146 | `/agents` monitor + live activity above composer + live transcripts | Specified | subagent-delegation › Agents monitor and live transcripts |
| AGT-147 | Steering a running subagent (model-facing and session-facing) | Specified | subagent-delegation › Control actions scoped to the caller's spawn tree |
| AGT-148 | Depth limit / orchestrator role | Specified | subagent-delegation › Role derived from depth only |
| AGT-149 | Finite chat runs auto-join children; messaging `delegation.gateway_mode` (auto/inline/background) without interim messages | Specified | subagent-delegation › Gateway delegation mode and one-shot joining |
| AGT-150 | Worktree isolation (`delegation.worktree_isolation`) | Specified | git-worktrees › Subagent worktree isolation |
| AGT-151 | Delegation vs execute_code | Specified | subagent-delegation › execute_code RPC channel and limits |
| AGT-152 | Cron creation via `/cron`, CLI, natural conversation; schedule formats (relative, interval, natural day/time, cron expr, ISO) | Specified | scheduled-jobs › Unambiguous schedule grammar; › CLI surface; › Model tool surface |
| AGT-153 | Skill-backed cron jobs (single/multiple skills) | Specified | scheduled-jobs › Job payload validation; › Fresh isolated agent session per run (skills preloaded) |
| AGT-154 | Per-job working directory (`--workdir`) loading project context | Specified | scheduled-jobs › Per-job working directory |
| AGT-155 | Per-job timezone, catch-up window | Specified | scheduled-jobs › Next-run computation; › Grace and missed occurrences |
| AGT-156 | Pin/unpin job model; `cron.model` for all unpinned jobs | Specified | scheduled-jobs › Model pinning and cron fleet default |
| AGT-157 | Create job paused (`--paused --paused-reason`) and global `hermes pause` | Specified | scheduled-jobs › Create paused with an auditable reason |
| AGT-158 | Agent-managed scheduling (`cron.allow_agent_scheduling`) | Specified | scheduled-jobs › Fresh isolated agent session per run |
| AGT-159 | Gateway scheduler, admission recovery, restart-safe workers | Specified | scheduled-jobs › At-most-once occurrence firing; › Built-in trigger inside the gateway daemon; › Durable delivery outbox |
| AGT-160 | Execution history (`hermes cron runs`) | Specified | scheduled-jobs › Run history |
| AGT-161 | Repeated-failure review nudge (`failure_streak`) | Specified | scheduled-jobs › Failure streak and review nudge |
| AGT-162 | Automatic re-runs when the model was unreachable (5/15/30 min, spend-neutral, not counted toward repeat) | Specified | scheduled-jobs › Automatic re-runs when the model was unreachable |
| AGT-163 | Hold job through a closed provider usage window (retry-after hint, whole chain unavailable) | Specified | scheduled-jobs › Hold through a closed provider usage window |
| AGT-164 | Failure incidents (alert once per error signature, reminder cooldown, acknowledge) | Specified | scheduled-jobs › Failure incidents with alert-once and acknowledge |
| AGT-165 | Cron doctor | Specified | scheduled-jobs › Cron doctor |
| AGT-166 | Delivery targets (origin, local, platform, chat:thread, all) | Specified | scheduled-jobs › Delivery targets |
| AGT-167 | Bot Chat delivery (`bot-chat[:<profile>]`, queued/claimed receipts) | Specified | scheduled-jobs › Bot Chat delivery target |
| AGT-168 | Push notification flag (`cron.delivery.notify`), Telegram cron topic, delivery confirmation `UNVERIFIED` | Specified | scheduled-jobs › Push notification flag, cron threads and delivery confirmation |
| AGT-169 | Continuable jobs (replying to a cron delivery continues that run's session) | Specified | scheduled-jobs › Continuable deliveries |
| AGT-170 | Response wrapping and `[SILENT]` suppression | Specified | scheduled-jobs › Response framing and silence |
| AGT-171 | Agent-declared failure (`[CRON_FAILURE]` first line) | Specified | scheduled-jobs › Agent-declared run failure |
| AGT-172 | No-agent script jobs, `wakeAgent:false` pre-run gate, script credentials, script timeout | Specified | scheduled-jobs › Script wake gate and script credentials |
| AGT-173 | Chaining jobs (`context_from`) | Specified | scheduled-jobs › Job chaining and continuity |
| AGT-174 | Manual runs asynchronous with result re-entering the conversation; per-run prompt | Specified | scheduled-jobs › Asynchronous tool-initiated runs with per-run context |
| AGT-175 | Cron toolsets from `hermes tools` cron platform | Specified | scheduled-jobs › Fresh isolated agent session per run (`cron.default_toolsets`) |
| AGT-176 | Pre-dispatch configuration validation | Specified | scheduled-jobs › Pre-dispatch preflight |
| AGT-177 | Automation blueprints (copy-paste patterns) and parameterised blueprint catalog with forms | Specified | scheduled-jobs › Automation blueprint catalog; scheduled-jobs › Suggested automations inbox |
| AGT-178 | Kanban boards, tasks, links, claim, dispatcher, workers, workspaces, attachments, notifications, runs, CLI | Specified | kanban › Board storage location … › CLI surface (all requirements) |
| AGT-179 | Idempotent create and bulk CLI verbs | Specified | kanban › Idempotent create; › CLI surface |
| AGT-180 | PR completion contracts (`--completion-contract OWNER/REPO\|PR URL\|local-only`, `metadata.published_pr`, branch-protection check) | Specified | kanban › PR completion contracts |
| AGT-181 | Goal-mode cards (`--goal`, `--goal-max-turns`, judge gate on complete/request-review) | Specified | kanban › Goal-mode cards |
| AGT-182 | Per-task model override and pinned extra skills | Specified | kanban › Per-task model override and pinned skills |
| AGT-183 | Respawn guard (blocker_auth, recent_success, active_pr) and `dispatch --dry-run` | Specified | kanban › Respawn guard and dispatch dry run |
| AGT-184 | Kanban swarm topology helper (`hermes kanban swarm`) | Specified | kanban › Swarm topology helper |
| AGT-185 | `/kanban` slash command (bypasses running-agent guard, auto-subscribe on create in gateway) | Specified | kanban › /kanban slash command |
| AGT-186 | Kanban dashboard (auto/manual orchestration, drag-to-delete, worker visibility REST endpoints, run terminate) | Specified | web-dashboard › Kanban dashboard API and orchestration modes |
| AGT-187 | Orchestrator profile behaviour and headless specify/decompose | Specified | kanban › Orchestrator guidance, specify and decompose |
| AGT-188 | Kanban worker lanes incl. external CLI lanes (non-spawnable assignee + spawn mechanism + terminator) | Specified | kanban › Worker lanes |
| AGT-189 | Multi-gateway deployments (single dispatcher, profile-owned notification delivery, notify+wake) | Specified | kanban › Subscription delivery modes and inheritance |
| AGT-190 | Kanban lifecycle plugin hooks (kanban_task_claimed/completed/blocked, worker spawned/exited/stale, task updated, dispatch tick) | Specified | plugin-system › Kanban lifecycle observer hooks |
| AGT-191 | Skills: SKILL.md format, progressive disclosure, slash invocation | Specified | skills › SKILL.md format…; › skills_list and skill_view tools; › Skill slash commands |
| AGT-192 | Stacking multiple skills in one command | Specified | skills › Stacking skills in one message |
| AGT-193 | `/learn` skill from sources; large sources become knowledge-base skills | Specified | skills › /learn distils a skill from sources |
| AGT-194 | Platform-specific and conditional (fallback) skills | Specified | skills › Platform, environment and conditional gates |
| AGT-195 | Skill media delivery and `[[as_document]]` | Specified | platform-adapters › Response-wide delivery modifiers and kanban artifacts |
| AGT-196 | Secure setup on load and skill config settings | Specified | skills › Skill secrets and settings |
| AGT-197 | Advisory SkillEvaluator scan | Specified | skills › Description budget and advisory lint on agent writes; › Skill authoring lint |
| AGT-198 | External skill directories and `skills.create_dir` | Specified | skills › Search roots and single precedence order |
| AGT-199 | Project-local skills with trust, quarantine, non-interactive surfaces | Specified | skills › Project-local skills require explicit trust |
| AGT-200 | Skill bundles (YAML bundle of skills, `hermes bundles`) | Specified | skills › Skill bundles |
| AGT-201 | Agent-managed skills (`skill_manage` actions) | Specified | skills › skill_manage tool validation; › Autonomous skill creation and improvement |
| AGT-202 | Skills Hub install/search/browse/inspect/update/audit, trust levels, `--force` | Specified | skills › Hub discovery, inspection, updates and audit |
| AGT-203 | Hub sources: official, skills.sh, well-known endpoints, GitHub, ClawHub, LobeHub, browse.sh, direct URL | Specified | skills › Hub source identifiers |
| AGT-204 | Custom skill taps (`hermes skills tap add\|list\|remove`, repo layout, tap trust) | Specified | skills › Custom skill taps |
| AGT-205 | Bundled skill updates (`hermes skills reset`) | Specified | skills › Bundled skills are read-only and ejectable |
| AGT-206 | Bundled and optional skill catalog content | Specified | skills › Bundled and official skill catalog content |
| AGT-207 | Curator (stale/archive, backups, rollback, pin, adopt, CLI) | Specified | skills › Curator configuration and scheduling; › Curator transitions, scope and backups; › Curator CLI |
| AGT-208 | Curator audit ledger with single-edit rollback, archive TTL purge, per-run reports with rename map, review on cheaper aux model | Specified | skills › Curator audit ledger, single-edit rollback, purge and reports |
| AGT-209 | Plugin kinds (general, memory provider, context engine, model provider) | Specified | plugin-system › Model-provider plugins; python-bridge › Model-provider plugins through the bridge |
| AGT-210 | Plugin discovery sources (bundled, user, project, pip entry points, Nix) | Specified | plugin-system › Python package and Nix plugin sources |
| AGT-211 | Opt-in enablement, allow-list, private repo installs | Specified | plugin-system › Opt-in enablement; › plugins CLI |
| AGT-212 | Plugin capabilities and consent | Specified | plugin-system › Capability declarations; › Capability grants are user-approved and digest-bound |
| AGT-213 | Plugin tools, hooks, slash commands, bundled skills, LLM access, MCP calls | Specified | plugin-system › Plugin tool registration; › Typed hook registry; › Plugin slash commands; › Plugin-bundled skills; › Plugin LLM access gate; › Host interfaces |
| AGT-214 | Plugin CLI subcommands (`ctx.register_cli_command` → `hermes <plugin> …`) | Specified | plugin-system › Plugin CLI subcommands; python-bridge › Plugin CLI commands through the bridge |
| AGT-215 | Approval transports (`register_approval_transport`) | Specified | plugin-system › Approval transports |
| AGT-216 | Plugin live message (`post_message` + edit) and `inject_message` | Specified | plugin-system › Plugin live messages and message injection; python-bridge › Host-bound plugin APIs through the bridge |
| AGT-217 | Platform actions (`ctx.platform_actions`, `gateway.platform_actions` capability) | Specified | plugin-system › Platform actions |
| AGT-218 | Plugin update checks and provenance (`.install-metadata.json`, `plugins adopt`) | Specified | plugin-system › Signed plugin packages and provenance; › plugins CLI (update) |
| AGT-219 | Dependency preparation and preservation | Specified | python-bridge › Per-plugin pinned environments |
| AGT-220 | One-click plugin install links (`hermes://` install URLs) | Specified | desktop-app › Install deep links open a consent flow (+ Deep links tightened) |
| AGT-221 | Plugin catalog (entries, trust tiers, live refresh, custom git URLs) | Specified | plugin-system › Plugin catalog; › Catalog kill list |
| AGT-222 | Plugin packs (`hermes-pack.yaml` pinning a set of plugins) | Specified | plugin-system › Plugin collections |
| AGT-223 | Install-time security scanning of plugins | Specified | plugin-system › Install-time threat scan |
| AGT-224 | Out-of-process plugins (`plugins.isolation: host`) | Specified | plugin-system › Python plugins route through the bridge (always out of process) |
| AGT-225 | Interactive `hermes plugins` UI, enabled/disabled/neither states | Specified | plugin-system › Opt-in enablement; › plugins CLI (browse) |
| AGT-226 | Plugins calling MCP servers (default-off per-server allowlist) | Specified | plugin-system › Host interfaces (tools.dispatch under tools_call capability) |
| AGT-227 | Built-in `disk-cleanup` plugin | Specified | plugin-system › First-party disk-cleanup plugin |
| AGT-228 | Built-in `security-guidance` plugin (25 rules, warn/block modes) | Specified | security-boundaries › Security guidance on written code |
| AGT-229 | Built-in `google_meet` plugin (join, captions transcript, meet_* tools, TTS say) | Specified | plugin-system › First-party google_meet plugin |
| AGT-230 | Built-in `hermes-achievements` dashboard tab | Specified | pets › Achievements from session history |
| AGT-231 | Langfuse (catalog) and NeMo Relay native observability integration | Specified | observability › NeMo Relay and Langfuse integrations |
| AGT-232 | Plugin hooks in Prometheus registry: pre/post_tool_call, pre/post_llm_call, api request observers, aux observers, session start/end/finalize/reset/delete, post_compaction, pre_gateway_dispatch, gateway_platform_event, pre_command, transform_terminal_output | Specified | plugin-system › Typed hook registry; › pre_tool_call directives; › pre_llm_call context preserves the prompt cache |
| AGT-233 | Hooks absent from Prometheus registry: transform_tool_result, transform_llm_output (+ hold_stream), on_stream_start/delta/end, on_interim_message, pre_verify, transform_api_error_classification, on_skill_lifecycle, subagent_start/stop, post_gateway_admission, agent_loop_stopped, pre/post_approval_*, on_human_input_request/resolved, on_room_member_activity, pre_transcription | Specified | plugin-system › Typed hook registry (tightened) + Transform hooks rewrite results and replies; Streaming and lifecycle observer hooks; pre_verify continuation hook; transform_api_error_classification hook; Approval and human-input observer hooks |
| AGT-234 | Cache-safe plugin system-prompt sections | Specified | plugin-system › Plugin system-prompt sections are bounded and frozen |
| AGT-235 | Gateway event hooks (`~/.hermes/hooks/<name>/HOOK.yaml` + handler.py; gateway:startup, session:*, agent:start/step/end, reaction:*, command:*; wildcards; BOOT.md tutorial) | Specified | plugin-system › Gateway hook directories |
| AGT-236 | Shell hooks (`hooks:` in config; JSON stdin/stdout; exit 2 blocks; matcher; first-use consent per (event, command); `hermes hooks` CLI; fail-open/closed) | Specified | plugin-system › Shell hooks with first-use consent; Hooks CLI |
| AGT-237 | Outbound webhooks (`hooks.outbound`: url, events, HMAC secret, matcher, timeout, retries) | Specified | plugin-system › Outbound webhooks |

## Install, operations, security and research tooling

| ID | Hermes feature | Status | Prometheus requirement |
|---|---|---|---|
| OPS-001 | POSIX source installer (`install.sh`, curl-pipe quick install) | Specified | distribution-and-update › Requirement: Installers |
| OPS-002 | Windows installer (`install.ps1`, `install.cmd` wrapper) | Specified | distribution-and-update › Requirement: Installers |
| OPS-003 | Installer skip flags for optional heavy deps (`--skip-browser`, `--skip-computer-use`, persisted as `pm install --without NAME`) | Specified | distribution-and-update › Persistent runtime opt-outs |
| OPS-004 | Desktop packages: macOS DMG + signed updater ZIP (Apple Silicon only), Windows MSIX bundle via `.appinstaller` (Win11 22H2+), Microsoft Store package with separate ownership, MSIX execution aliases `hermes`/`hermes-agent`/`hermes-acp` | Specified | distribution-and-update › Desktop packages and signing (+ Install receipt owner list tightened) |
| OPS-005 | Android / Termux install (prebuilt package, `pkg install`, documented limits: no browser bundle, no Docker, no voice) | Specified | distribution-and-update › Release artifacts and targets (aarch64 Termux) + Package-manager and image channels (Termux package, owner `termux`); workspace-architecture › Supported targets and support tiers (Tier 2, weaker capabilities reported by doctor) |
| OPS-006 | Nix flake package + overlay | Specified | distribution-and-update › Package-manager and image channels (Nix flake, owner `nix`); Reproducible, locked builds |
| OPS-007 | NixOS module (`services.hermes-agent`: declarative settings, native vs container mode, sops-nix/agenix secrets, documents, MCP servers, `extraPlugins`/`extraPythonPackages`, managed mode, GC-root protection) and Home Manager module (`programs.hermes-agent`, backend/webapp services) | Specified | distribution-and-update › NixOS and Home Manager modules |
| OPS-008 | Official Docker image: entrypoint modes (gateway, dashboard, webapp, interactive CLI), `/opt/data` volume, s6 per-profile gateway supervision, multi-profile in one container, `docker exec` drops to `hermes` user, tini/zombie reaping, healthcheck, resource limits, Compose example, local inference connectivity | Specified | distribution-and-update › Container image run modes; Container filesystem, init and health; Multiple profiles in one container; Container audio bridge and host services |
| OPS-009 | Image provenance marker / immutable install tree (update refused inside image, points to `docker pull`) | Specified | distribution-and-update › Uniform refusal for installs owned elsewhere (owner `image`, exit 69) |
| OPS-010 | Install-method auto-detection (source/desktop/docker/nix/termux owner) | Specified | distribution-and-update › Install receipt |
| OPS-011 | `hermes update --install-id` (print install identity and path) | Specified | distribution-and-update › Install receipt; Doctor reports distribution health (owner, channel, version) |
| OPS-012 | Platform support tiers (Tier 1/Tier 2/unsupported, Intel macOS unsupported) | Specified | workspace-architecture › Supported targets and support tiers |
| OPS-013 | Windows-native operation: terminal tool runs commands through Git Bash (resolver, WindowsApps caveat), UTF-8 console, Ctrl+Enter newline | Specified | execution-environments › Windows local shell |
| OPS-014 | Run gateway at OS login without admin (Windows Scheduled Task `ONLOGON /RL LIMITED` with Startup-folder fallback, detached no-console spawn; systemd user unit / launchd agent on POSIX; non-sudo service user installs) | Specified | messaging-gateway › Per-OS service installation without elevation |
| OPS-015 | WSL2 usage (systemd in WSL, `/mnt/c` vs Linux FS, Windows↔WSL networking) | Specified | Linux targets in workspace-architecture › Supported targets; session-storage › WAL mode, bounded readers and checkpoints (refuses WAL on cross-VM filesystems) |
| OPS-016 | Switching from a bundled install to a source checkout (independent checkout, data selection, switch back without downgrade) | Specified | distribution-and-update › Install receipt (`owner = dev` for source builds) and Uniform refusal; Schema migrations around updates (newer store refused by older binary) |
| OPS-017 | `hermes update` core flow (fetch, dependency sync, restart, exit codes) | Specified | distribution-and-update › Atomic binary swap with health check; Update exit codes; Single-writer update lock |
| OPS-018 | Update channels (`stable`/`main`, `--set-channel`, `--channel` one-shot) | Specified | distribution-and-update › Release channels (stable/canary) |
| OPS-019 | `hermes update --check` (preview without installing) | Specified | distribution-and-update › Update exit codes (10 for `--check`) |
| OPS-020 | `hermes update --plan` (fleet plan: install kind, every running service across profiles, restart method) | Specified | distribution-and-update › Update dry-run and changelog preview (upgrade) (daemon restart needed, per-profile migrations) |
| OPS-021 | Gateway restart after update, `--no-gateway-restart`, interrupted-restart recovery, manual restart reminders | Specified | distribution-and-update › Running daemon handover (`--no-restart`, `daemon_restart_pending` follow-up) |
| OPS-022 | Update receipts + fleet version check (`logs/update_receipts/`, `latest.json`) | Specified | distribution-and-update › Update receipts at the root home |
| OPS-023 | Pre-update backup (`updates.pre_update_backup: quick\|full\|off`, `--backup`, `--no-backup`) | Specified | distribution-and-update › Pre-update backup |
| OPS-024 | Rolling back to an older release | Specified | distribution-and-update › Rollback |
| OPS-025 | Passive update notices | Specified | distribution-and-update › Update check without telemetry |
| OPS-026 | `/update` from messaging platforms (gateway-driven update with progress streamed to chat, `--gateway` IPC mode) | Specified | messaging-gateway › Restart and update from chat |
| OPS-027 | Source-checkout update mechanics (`--branch`, `--switch-branch`, auto-stash, `--keep-stash`, npm lockfile churn, partial-clone repair, `--force-venv`, `--list-venv-holders`, Python 3.14) | N/A | Prometheus ships one signed binary; there is no git checkout or venv to update (design D-distribution; `owner = dev` builds are refused by `update`). |
| OPS-028 | Desktop bundle updates and macOS bundle update hand-off | Specified | desktop-app › Signed in-app updates; Development builds have no updater |
| OPS-029 | `hermes uninstall` (`--full`, `--data` data-only, `--gui`, `--desktop-userdata`, `--dry-run`, `--yes`; sealed installs defer to package owner) | Specified | distribution-and-update › Uninstall scopes and dry run |
| OPS-030 | Manual uninstall instructions | Specified | distribution-and-update › Uninstall + Install receipt (binary path recorded) |
| OPS-031 | `hermes backup` whole-home zip (`-o`, `--quick` critical-state snapshot with `--label`, `--keep N` default 3, SQLite online backup API, excludes WAL sidecars/regenerable runtimes/browser profiles/caches/sockets, exit 1 on partial, 2 if another backup running) | Specified | configuration-and-profiles › Whole-home backup |
| OPS-032 | `hermes import <zip>` restore (CRC-verify every member before writing, `--force`, page-copy restore of live `.db` files, warns when older backup replaces newer data, skip list) | Specified | configuration-and-profiles › Home restore |
| OPS-033 | Quick state snapshots and `/snapshot list\|restore`, `hermes backup restore --state pre-update` | Specified | configuration-and-profiles › State snapshots |
| OPS-034 | `hermes pm install/status/doctor/repair/gc/env` (managed tool store, digests vs lock, generations, leases) | Specified | distribution-and-update › Managed runtime store; Runtime CLI; Runtime leases and garbage collection |
| OPS-035 | `hermes pm environment NAME --extra/--requirement` (isolated side Python environments for skills, prints interpreter path) | Specified | python-bridge › Named side environments |
| OPS-036 | PM network retry policy (4 attempts, 1/2/4 s, Retry-After ≤30 s, range resume, mirror fallback) | Specified | distribution-and-update › Download retry budget |
| OPS-037 | Lazy install of optional dependencies gated by `security.allow_lazy_installs` | Specified | distribution-and-update › Lazy runtime installation policy |
| OPS-038 | Dependency release-age gate (≥3-day-old packages in lock, CA-store exemption) and mirror support | Specified | distribution-and-update › Dependency release-age gate; Download mirrors |
| OPS-039 | `hermes pm lock/update/bundle` maintainer commands, activate scripts, dev workflow | N/A | Hermes Python dependency tooling; Prometheus uses the Cargo lockfile and `scripts/check` (workspace-architecture › Repository health gates). Offline bundling is covered by distribution-and-update › Offline bundles. |
| OPS-040 | Stable release process (lanes, tag namespaces, canary identities, R2 dynamic channels, signed-package baseline) | Specified | distribution-and-update › Release identity grammar; Signature trust root; Release artifacts and targets |
| OPS-041 | SECURITY.md trust model and private reporting (§2 boundaries, §3.1 private vs §3.2 public) | Specified | security-boundaries › Declared trust model; In-scope findings and private reporting |
| OPS-042 | Approval modes (manual/smart/off), YOLO | Specified | tool-system › Approval modes are typed; Smart approvals with a denial breaker |
| OPS-043 | Hardline blocklist + user `approvals.deny` rules | Specified | tool-system › Approval floors cannot be bypassed |
| OPS-044 | Supervised-gateway lifecycle guard (terminal tool refuses stopping/restarting its own gateway and interpreter-image kills like `pkill python`, not bypassable by approval/yolo) | Specified | tool-system › Self-termination guard |
| OPS-045 | Approval timeout, CLI and gateway approval flows, permanent allowlist | Specified | tool-system › Approval channel fails closed; Human approval choices and grants file; messaging-gateway › Approval requests routed to every surface |
| OPS-046 | `execute_code` scripts go through approval and env stripping | Specified | subagent-delegation (execute_code tool, OS sandbox); execution-environments › Child environments are built from an allowlist |
| OPS-047 | Content-level dangerous-command checks (curl/wget uploading secret vars or credential files; invisible/bidi Unicode in commands) | Specified | tool-system › Content-level dangerous-command detectors |
| OPS-048 | `hermes approvals suggest` (mine implied approvals from session history into allowlist proposals; `--days` 90, `--min-count` 2, `--limit` 20, `--apply N`, `--db`) | Specified | tool-system › Approval suggestions mined from history |
| OPS-049 | `hermes approvals test <cmd>` (dry-run verdict through the real guard order, `--env-type` backend, never executes) | Specified | tool-system › Approval dry-run test command |
| OPS-050 | File write safety: protected credential paths (OS credential stores, Hermes secret stores, Windows NT/device namespace paths), read-deny of project `.env*`/`.envrc`, `~/.ssh/config` approval-gated | Specified | execution-environments › Protected secret files and device-namespace paths |
| OPS-051 | `HERMES_WRITE_SAFE_ROOT` write sandbox (multi-root) | Specified | security-boundaries › Typed capability vocabulary (`fs.write:<glob>`) and Capability resolution never broadens (default write = session workspace + scratch) |
| OPS-052 | Gateway user authorization, platform allowlists, DM pairing | Specified | messaging-gateway › Authorization order and fail-closed default; Documented pairing flow; security-boundaries › External surfaces fail closed |
| OPS-053 | Container isolation flags, resource limits, filesystem persistence; backend security comparison | Specified | execution-environments › Container hardening is never silently dropped; Backend isolation descriptor |
| OPS-054 | Environment variable passthrough (strip secrets from children; skills' `required_environment_variables` pass through) | Specified | execution-environments › Child environments are built from an allowlist; skills (declared secrets) |
| OPS-055 | Credential file passthrough (`required_credential_files` in skill frontmatter mounted read-only into Docker / synced into Modal) | Specified | skills › Skill credential file passthrough |
| OPS-056 | Borrowed CLI logins (`auth.adopt_external_logins`) | Specified | model-providers › Borrowed CLI logins |
| OPS-057 | MCP credential handling (safe env vars for stdio servers, credential redaction in MCP errors) | Specified | tool-system › MCP runtime safety; security-boundaries › Known-secret redaction everywhere |
| OPS-058 | Website access policy (`security.website_blocklist` domains + shared blocklist files, enforced across web/browser tools) | Specified | security-boundaries › Website blocklist |
| OPS-059 | SSRF protection (private/loopback/link-local/CGNAT/metadata blocked, DNS failure fails closed, redirects re-validated per hop, remote-supplied URLs also checked) | Specified | security-boundaries › In-process fetch SSRF protection |
| OPS-060 | Context file prompt-injection scanning (AGENTS.md, .cursorrules, SOUL.md) | Specified | agent-turn-loop › Context file scanning and size cap |
| OPS-061 | TLS trust via OS trust store (truststore; corporate proxy roots via OS) | Specified | security-boundaries › Platform trust store for TLS |
| OPS-062 | Trusted-by-placement gateway event hooks (directory placement = opt-in) | Specified | plugin-system › Opt-in enablement (Prometheus deliberately requires explicit enablement for all extension points, closing this exception) |
| OPS-063 | Supply-chain advisory catalog (known-compromised package versions flagged in startup banner, doctor, gateway log; `hermes doctor --ack <id>` persisted in `security.acked_advisories`) | Specified | security-boundaries › Supply-chain advisory catalog |
| OPS-064 | `hermes security audit` (on-demand OSV.dev scan of venv, plugin requirements, pinned npx/uvx MCP servers; `--fail-on` low\|moderate\|high\|critical default critical, `--skip-venv/--skip-plugins/--skip-mcp`, `--json`) | Specified | security-boundaries › Dependency vulnerability scan |
| OPS-065 | Startup security posture audit (warn-on-load, never blocks) | Specified | security-boundaries › Hardening downgrades are never silent; Security posture check; Private file modes (WARN on loose modes) |
| OPS-066 | Secret redaction in tool output, logs, uploads | Specified | security-boundaries › Known-secret redaction everywhere; observability › Trajectories are lossless and redacted |
| OPS-067 | External secret sources: Bitwarden Secrets Manager and 1Password (`hermes secrets bitwarden\|onepassword` setup wizard, preview, status, token rotation, caching, fail-loud) | Specified | security-boundaries › External secret sources are opt-in; configuration-and-profiles › Secret resolution precedence and scope |
| OPS-068 | Command-helper secret source (`secrets.command`: run a helper once at startup via `/bin/sh -c`, parse KEY=VALUE, 3 s timeout, 1 MiB cap, stderr discarded, `override_existing` false, POSIX-only) | Specified | security-boundaries › Command secret source |
| OPS-069 | Secret-source plugin kind (custom vault backends) | Specified | plugin-system (provider world registers secret-source providers) |
| OPS-070 | Managed scope (`/etc/hermes/config.yaml` + managed `.env` pinning keys, relocatable via `HERMES_MANAGED_DIR`, refusal message names source, `hermes config`/`doctor` show managed keys) | Specified | configuration-and-profiles › Managed secrets and managed visibility |
| OPS-071 | Egress credential-injection proxy (iron-proxy) for remote/docker sandboxes: `hermes egress install/setup/start/stop/restart/reload/status/disable/config`, Bitwarden as credential source, token rotation, CA distribution into sandbox, allowed upstream hosts, SSRF deny CIDRs | Specified | security-boundaries › Egress proxy coverage of containers and remote sandboxes (+ Egress proxy operation and status; Egress credential source and placeholder rotation) |
| OPS-072 | Docker network egress isolation guide (internal network + egress proxy, Compose) | Specified | security-boundaries › Per-session network egress policy (upgrade) (`allowlist`/`none`) and Egress enforcement for child processes |
| OPS-073 | Secure-on-a-work-machine posture (cautious starting config, checklist) | Specified | security-boundaries › Security posture check (`prometheus doctor security --strict`) |
| OPS-074 | Windows owner-only ACLs on Hermes home | Specified | security-boundaries › Private file modes |
| OPS-075 | `hermes logs` (agent/errors/gateway/gui/desktop/mcp/update/handoff, `list`, `-n` 50, `-f`, `--level`, `--session`, `--since`, `--component`, rotation) | Specified | cli › Log viewer; observability › Structured logs |
| OPS-076 | `hermes status` (`--full`/`--all`, `--deep`; model, providers with creds, gateway, platforms, cron jobs; keys never printed) | Specified | observability › Status command |
| OPS-077 | `hermes doctor` (`--fix`, exit 1 on unresolved, IPv6 blackhole check → `network.force_ipv4`, config-shape checks, `--live` real-call probes, `--ack`) | Specified | workspace-architecture › IPv6 reachability check and IPv4-only outbound (+ Doctor gains `network` group) |
| OPS-078 | `hermes usage` (account rate-limit windows per provider, `--provider`, `--json` stable schema, no session needed) | Specified | model-providers › Subscription plan and quota visibility (upgrade) (`prometheus auth status <provider>`, `/usage`). Note: Prometheus `usage` command is the cost ledger (observability › Usage command), so the Hermes name maps to `auth status`. |
| OPS-079 | `hermes insights` / `/insights` (last N days default 30, `--source` filter: overview, cost estimate, model and platform breakdown, tool and skill usage, activity patterns by weekday/hour, top sessions) | Specified | observability › Usage insights report |
| OPS-080 | `hermes journey` (aliases `learning`, `memory-graph`; terminal timeline of learned skills + memories, playable constellation scrubber; mirrors TUI `/journey` and desktop Star Map) | Specified | memory › Learning journey timeline |
| OPS-081 | `hermes prompt-size` (offline byte breakdown of fixed per-call payload: system prompt total, skills index, memory, user profile, prompt tiers stable/context/volatile, tool schemas; `--platform`, `--json`) | Specified | agent-turn-loop › Offline prompt size report |
| OPS-082 | `hermes dump` (plain-text copy-paste setup summary: version/commit, OS, profile, home, model/provider, terminal backend, API-key presence, toolsets, MCP count, memory provider, gateway & platforms, cron/skill counts, non-default config overrides with credentials redacted; `--show-keys` prefixes) | Specified | observability › Plain-text setup summary |
| OPS-083 | `hermes debug share` (upload redacted report to paste.rs/dpaste with `--expire` days default 7, `--lines` 200, `--local`, `--no-redact`, `--nous` private upload, `debug delete`) | Specified | observability › Opt-in debug share upload (+ Crash and debug bundle / No outbound telemetry tightened) |
| OPS-084 | Gateway monitoring OTLP export (gateway gauges, lifecycle traces, content-free diagnostics logs, cron scheduler gauges and execution lifecycle; hashed install id; `monitoring.export.otlp.headers_env`) | Specified | messaging-gateway › Gateway health export is opt-in and content-free; observability › Opt-in OpenTelemetry export; Metrics catalog |
| OPS-085 | `hermes monitoring status` (monitoring settings, export state, redaction posture) | Specified | workspace-architecture › Doctor (`observability` group); observability › Component health model (OTLP exporter state) |
| OPS-086 | Relay shared metrics (consented remote exporter of decision metrics, install-id, reset/deletion) | Specified | security-boundaries › Telemetry is opt-in and HTTPS-only; observability › No outbound telemetry by default |
| OPS-087 | Langfuse tracing (catalog plugin) | Specified | observability › Opt-in OpenTelemetry export (Langfuse ingests OTLP); plugin-system › Typed hook registry for a dedicated plugin |
| OPS-088 | Session storage recovery (WAL-holder refusal, doctor lists holder PIDs, maintenance commands refuse under a live writer with `--force`/`--dry-run`) | Specified | workspace-architecture › Single-writer storage invariant; session-storage › Integrity check and quarantine; Out-of-band file replacement detection; Derived indexes fail open and rebuild offline |
| OPS-089 | `hermes checkpoints` (status/list `--limit`, prune `--retention-days 7` `--max-size-mb 500` `--keep-orphans`, clear, clear-legacy) for the `/rollback` shadow store | Specified | execution-environments › Snapshot store management CLI |
| OPS-090 | Batch runner (`batch_runner.py`: JSONL prompt dataset, `--batch_size`, `--num_workers` 4, `--max_turns` 10, `--max_samples`, `--resume` checkpoint, provider routing flags, reasoning effort, `--ephemeral_system_prompt`, `--prefill_messages_file`, per-prompt container image, quality filtering, `statistics.json`) | Specified | replay-and-evaluation › Batch prompt datasets; Batch statistics and quality filtering |
| OPS-091 | Toolset distributions (named distributions giving per-toolset inclusion probability, compound `a+b` keys, sampled per prompt) | Specified | replay-and-evaluation › Toolset distributions |
| OPS-092 | Trajectory saving in ShareGPT from/value format (`<tool_call>`/`<tool_response>` normalization, reasoning markup, HuggingFace loading) | Specified | observability › ShareGPT trajectory export |
| OPS-093 | Trajectory compressor (post-process trajectories into a token budget for fine-tuning: protect head/tail, summarize middle, `--target_max_tokens`, `--sample_percent`) | Specified | replay-and-evaluation › Trajectory compressor |
| OPS-094 | Mini SWE runner (run tool-calling tasks in local/docker/modal envs, single or JSONL batch, Hermes trajectory output) | Specified | replay-and-evaluation › Benchmark adapter protocol; Eval run (per-task fresh workspace on a chosen backend) |
| OPS-095 | `evals/` regression probes | N/A | Hermes-internal regression probes for specific bugs, not a user feature; Prometheus has its own eval suites (replay-and-evaluation › Built-in smoke suite). |
| OPS-096 | Python library embedding (`from run_agent import AIAgent`; `chat()`, `run_conversation()`, `enabled_toolsets`, `ephemeral_system_prompt`, `quiet_mode`, `save_trajectories`, multi-turn history, FastAPI/Discord/CI examples) | Specified | workspace-architecture › Embeddable client SDKs |
| OPS-097 | ACP for editors | Specified | acp-adapter (whole capability) |
| OPS-098 | TUI gateway JSON-RPC (method catalog, model overrides on `session.create`, rewind on `prompt.submit`, events, server→client requests, reconnect rebuild) | Specified | workspace-architecture › JSON-RPC framing and error codes; Server-to-client requests; Event sequencing and reconnect replay |
| OPS-099 | OpenAI-compatible API server (`/v1/chat/completions`, `/v1/responses`, `/v1/models` model catalog) | Specified | api-server › Opt-in listener that fails closed; api-server › Chat Completions endpoint; api-server › Responses endpoint and response store; api-server › Model listing and Open WebUI compatibility |
| OPS-100 | `hermes chat --format stream-json` JSONL event output and `--usage-file` JSON usage report | Specified | cli › Streaming JSONL output; cli › Usage report file |
| OPS-101 | `hermes verify [path]` (detect project run recipe: bootstrap/build/test/start phases; `.hermes/environment.json` manifest wins; `--detect-only` JSON, `--save`, `--skip-start`, `--phase` repeatable, `--port`, `--timeout` 600 s/phase, `--ready-timeout` 60 s; records evidence for the turn-end verification guard) | Specified | execution-environments › Project run-recipe verification |
| OPS-102 | `hermes -w` automatic worktree mode (disposable `.worktrees/` tree + `hermes/hermes-<hash>` branch, repo hooks/fsmonitor/filters disabled, exit prompt keep/archive/discard for unfinished work) and `/worktree new` | Specified | git-worktrees › chat -w worktree mode (+ Exit disposition; In-session worktree command) |
| OPS-103 | `hermes worktree list\|prune\|restore` and stale-worktree sweep (registry of repos, tiers 24 h for agent trees / 72 h named, archive before removing dirty trees, never touch live trees; `--older-than`, `--dry-run`, `--trees-only`, `--branches-only`, `--json`, restore `--path`) | Specified | git-worktrees › Worktree CLI and restore; git-worktrees › Repository registry and sweep schedule; git-worktrees › Sweep decision table; git-worktrees › Archive format |
| OPS-104 | UI language packs / i18n (`display.language`, 17 bundled locales, per-profile overlay `<home>/locales/<lang>.yaml`, plugin language packs via `provides_locales`, partial packs with English fallback, validation of language ids, endonym lists in TUI/desktop) | Specified | i18n › display.language selection; Bundled languages; Language switchers show endonyms; Migration of Hermes language settings |
| OPS-105 | Import from other agents (OpenClaw, Claude Code, Codex CLI, Cursor via `hermes import-agent` / `hermes claw migrate`) | Specified | hermes-migration › Importers for other agents; Other agents never yield secrets by default; Claude Code, Codex CLI and Cursor mappings; OpenClaw mapping; Source archiving after import; Keeping imports in sync |
| OPS-106 | Local encrypted autofill vault (`hermes vault add\|list\|rm\|key migrate --to keychain\|file`, password-blind browser autofill) | Specified | security-boundaries › Credential vault storage; security-boundaries › browser_vault fills by handle; security-boundaries › Vault management CLI |

