# Local Models

## Purpose
Local models let Prometheus run open-weight models entirely on the user's machine: it installs a hash-pinned llama.cpp engine for the detected hardware, prices a curated catalog against that hardware, downloads models resumably, and supervises a loopback `llama-server` whose context window and memory placement it manages. The key decision is that the managed runtime is just the endpoint behind the ordinary `llamacpp` provider of `model-providers`, so selection, fallback, accounting (`billing_source = "local"`) and the prompt-cache invariant are unchanged, while an externally run llama-server is detected and preferred over starting a second one.

## ADDED Requirements

### Requirement: Local runtime configuration
The system SHALL read machine-wide runtime settings from `[local_runtime]` in `<PROMETHEUS_HOME>/daemon.toml` with keys `enabled` (default false: detection only, never starts a server), `backend` (`auto` default, `cuda`, `metal`, `vulkan`, `hip`, `cpu`), `models_max` (default 4, range 1-16, models resident at once), `port` (default 0 = pick a free loopback port at spawn), `detect_ports` (default empty; extra ports probed besides 8080) and `idle_unload_secs` (default 900), rejecting unknown keys and values with exit 78, while each profile selects a local model with the standard `model.provider = "llamacpp"` and `model.default`.
**Milestone:** M2

#### Scenario: Invalid backend
- **GIVEN** `[local_runtime] backend = "rocm"`
- **WHEN** the daemon config loads
- **THEN** the system SHALL fail validation listing the accepted backends and exit 78

#### Scenario: Detection-only default
- **GIVEN** default `[local_runtime]` and no external server
- **WHEN** a session requests `llamacpp`
- **THEN** the system SHALL NOT start a server and SHALL fail with `provider.local_runtime_down`

### Requirement: Hash-pinned engine installation
The system SHALL install the llama.cpp engine (and every dependent library, including CUDA runtime libraries) only from the URLs and SHA-256 pins of the release's runtime manifest into `<PROMETHEUS_HOME>/runtimes/llamacpp/<version>-<backend>/`, never downloading an unpinned or "latest" build and accepting no engine-version override in config, SHALL verify every archive before unpacking, SHALL refuse an unsupported (backend, target) pair before any download with exit 69 naming the pair, and SHALL never download during a boot or session start (an installed engine is used as-is; a newer pin is installed only by an explicit install or update command).
**Milestone:** M2

#### Scenario: Corrupted archive rejected
- **GIVEN** an engine archive whose SHA-256 does not match its pin
- **WHEN** the install verifies it
- **THEN** the system SHALL delete the archive, install nothing, and exit 74 naming the file

#### Scenario: HIP on macOS refused
- **GIVEN** `backend = "hip"` on macOS
- **WHEN** the user runs `prometheus local install`
- **THEN** the system SHALL exit 69 before any network request

### Requirement: Backend auto-selection
The system SHALL resolve `backend = "auto"` to CUDA for NVIDIA GPUs (Linux x86_64/aarch64 with a CUDA 13-capable driver, Windows), Metal on macOS, Vulkan for AMD and Intel GPUs on Linux and Windows x86_64, and CPU when no supported GPU is found, choosing HIP only when explicitly configured on supported x86_64 targets, and SHALL size a discrete GPU from its own memory minus memory currently held by other programs and an integrated GPU from system RAM.
**Milestone:** M2

#### Scenario: AMD GPU on Linux
- **GIVEN** an AMD discrete GPU on Linux x86_64 and `backend = "auto"`
- **WHEN** the engine is installed
- **THEN** the system SHALL install the Vulkan build

### Requirement: Hardware-priced model catalog
The system SHALL ship a curated model catalog and price every entry against the probed hardware before download, showing for each the memory fit (`fits_gpu`, `uses_system_ram`, `too_big`), starting and maximum context window, and the download size of the build selected for this machine, choosing the highest-quality quantisation that runs entirely in GPU or unified memory and otherwise a smaller quantisation, never offering builds below 4-bit, keeping `too_big` entries visible with the reason, and recommending the highest-quality model that fits entirely in GPU memory at a predicted 20 or more tokens per second (or the vendor-default model from the build's distribution config when it fits), recommending none when no model reaches that floor.
**Milestone:** M2

#### Scenario: Catalog on an 8 GB GPU
- **GIVEN** an 8 GB discrete GPU
- **WHEN** the user runs `prometheus local catalog --json`
- **THEN** each entry SHALL carry `fit`, `context_start`, `context_max`, `quant` and `download_bytes`, a 35B model SHALL be `uses_system_ram` or `too_big`, and no entry SHALL offer a quantisation below 4-bit

### Requirement: Resumable, verifiable downloads
The system SHALL download engine components, model weights, split GGUF parts, vision projectors and draft models through one downloader that writes partial files under `<PROMETHEUS_HOME>/cache/partials/`, resumes missing byte ranges from hosts that support HTTP Range (restarting only the current file otherwise), reuses completed files, verifies sizes and digests before publishing a file into the library (never publishing an incomplete file), reports byte progress for the whole download plan including completed and resumed bytes, and supports `pause` and `resume` of any job at a chunk boundary with paused jobs persisted across restarts.
**Milestone:** M2

#### Scenario: Resume after interruption
- **GIVEN** a 10 GB model download paused at 6 GB from a Range-capable host
- **WHEN** the user runs `prometheus local resume <job>`
- **THEN** the system SHALL request only the remaining bytes and report progress starting at 60%

#### Scenario: Interrupted download not usable
- **GIVEN** a download that stopped before completion
- **WHEN** the user lists the model library
- **THEN** the model SHALL appear as `partial`, not as usable

### Requirement: Model library
The system SHALL store downloaded models in the machine-shared `<PROMETHEUS_HOME>/models/` directory usable by every profile, SHALL let `prometheus local add <path.gguf>` link an existing GGUF file into the library without copying it, SHALL let `prometheus local search <query>` search Hugging Face with download counts and a per-file fit check for this machine (community models carry no `validated` badge), SHALL read each model file's own metadata to choose its context window and memory placement, and SHALL remove every staged file of a model (weights, parts, vision projector, draft model) on `prometheus local rm <model>` while leaving a linked original file untouched.
**Milestone:** M2

#### Scenario: Linked file not deleted
- **GIVEN** a GGUF added with `prometheus local add ~/models/x.gguf`
- **WHEN** the user runs `prometheus local rm x`
- **THEN** the library entry SHALL be removed and `~/models/x.gguf` SHALL still exist

### Requirement: Managed server lifecycle
The system SHALL, when `local_runtime.enabled = true` and a profile's route resolves to `llamacpp` without an explicit `base_url`, start one managed `llama-server` per Prometheus home bound to `127.0.0.1` on `local_runtime.port` (or a free port), supervised by the gateway daemon when it runs and otherwise by the first client process with a runtime record (pid and start time) under `<PROMETHEUS_HOME>/run/` so other processes reuse it, keeping at most `models_max` models resident, unloading a model idle for `idle_unload_secs` and reloading it on the next request, restarting a crashed server with backoff (1 s doubling to 60 s, 5 attempts) and stopping it when its supervisor exits.
**Milestone:** M2

#### Scenario: Second process reuses the server
- **GIVEN** a managed server started by one CLI process
- **WHEN** a second CLI process starts a session on `llamacpp`
- **THEN** the second process SHALL connect to the recorded server and SHALL NOT start another

#### Scenario: Idle model unloaded
- **GIVEN** a resident model with no request for 900 seconds under default config
- **WHEN** the idle timer fires
- **THEN** the model SHALL be unloaded and the next request SHALL reload it before answering

### Requirement: Context window and memory placement
The system SHALL launch each local model at the largest context window that fits fully in GPU memory counting weights, KV cache, runtime buffers, vision projector and multi-token-prediction buffers (using smaller batches rather than disabling multi-token prediction when larger batches would spill), SHALL guarantee at least 65536 tokens for every recommended model by placing overflow in system RAM in least-harmful order (expert weights first, never the KV cache), SHALL grow the window toward the model's native maximum when the conversation needs more room, emitting a `local_model.context_grown` event and re-reporting the window to the context gauge, and SHALL report to `context-compression` that growth is impossible (no fit, generation below the speed floor, or native maximum reached) so compression runs instead of claiming a window the server did not receive.
**Milestone:** M3

#### Scenario: Window grows before compression
- **GIVEN** a local session at a 65536-token window with room to grow to 131072 in memory
- **WHEN** the conversation reaches the compression threshold
- **THEN** the system SHALL relaunch the model at a larger window, emit `local_model.context_grown`, and SHALL NOT compress

#### Scenario: Growth impossible triggers compression
- **GIVEN** a local session at the model's native maximum window
- **WHEN** the conversation reaches the compression threshold
- **THEN** the system SHALL compress normally

### Requirement: External llama-server detection
The system SHALL, when a route resolves to `llamacpp` without an explicit `base_url`, first probe `127.0.0.1:8080` and each port in `local_runtime.detect_ports` for a llama-server (via `/props` and `/v1/models`) and use the first one found instead of starting a managed server, and SHALL let an explicit `[providers.llamacpp] base_url` override both detection and the managed runtime.
**Milestone:** M2

#### Scenario: User-run server preferred
- **GIVEN** a llama-server the user runs on port 8081, `detect_ports = [8081]`, and `enabled = true`
- **WHEN** a session starts on `llamacpp`
- **THEN** the system SHALL use the server on 8081 and SHALL NOT start a managed server

### Requirement: Local runtime errors
The system SHALL fail a `llamacpp` route that resolves to no reachable server with code `provider.local_runtime_down` and the message `the local model server isn't running` followed by the remedy (`prometheus local start`, enabling `local_runtime.enabled`, or the detected-port hint), never with `provider.unknown` or `provider.missing_credential`, and SHALL apply the `model-providers` rule that a local route never falls back to a remote provider unless the fallback chain names one.
**Milestone:** M2

#### Scenario: Server down with a remote key present
- **GIVEN** `model.provider = "llamacpp"`, no reachable server, and an `OPENROUTER_API_KEY` secret
- **WHEN** a non-interactive turn runs
- **THEN** the system SHALL fail with `provider.local_runtime_down`, send no request to OpenRouter, and exit 69

### Requirement: Local models command
The system SHALL provide `prometheus local status [--json]` (engine version and backend, server state and port, resident models, GPU/RAM usage), `install`, `update`, `catalog [--json]`, `pull <model>`, `pause <job>`, `resume <job>`, `jobs`, `search <query>`, `add <path>`, `rm <model>`, `use <model> [--global]` (sets the route to `llamacpp/<model>` for the session or profile) and `start|stop`, plus a `Local models` entry in `prometheus model` that runs the quickstart (show the hardware-priced catalog with the recommendation marked, install the pinned engine, download with live progress where Ctrl+C pauses and re-selecting resumes, start the server, set the model as default), and SHALL expose the same operations as `local.*` RPC methods for the desktop and dashboard.
**Milestone:** M2

#### Scenario: Ctrl+C pauses a pull
- **GIVEN** `prometheus local pull qwen-x` in progress on a terminal
- **WHEN** the user presses Ctrl+C
- **THEN** the job SHALL be paused (not deleted) and `prometheus local jobs` SHALL list it as `paused`

### Requirement: Local inference stays local
The system SHALL send no network request for a local model once its files are installed (engine, weights and catalog metadata are read from disk), SHALL record local calls with `billing_source = "local"` and `cost_status = "exact"` at zero cost, and SHALL make catalog refreshes and Hugging Face searches explicit user actions only.
**Milestone:** M2

#### Scenario: Offline chat
- **GIVEN** an installed engine and model and no network connectivity
- **WHEN** a session runs a turn on `llamacpp`
- **THEN** the turn SHALL complete and the ledger row SHALL carry `billing_source = "local"`

### Requirement: Local resource telemetry
The system SHALL publish GPU utilisation, GPU memory and system RAM usage of the local runtime through the session status RPC at most once per 2 seconds while a local model is resident, for display in the TUI and desktop status bars, and SHALL show the context gauge using the window the server actually runs with.
**Milestone:** M3

#### Scenario: Gauge shows real window
- **GIVEN** a local model launched at 65536 tokens whose catalog maximum is 262144
- **WHEN** the status bar renders
- **THEN** the context gauge SHALL use 65536 as the window
