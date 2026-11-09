# Local models

Prometheus can run open-weight models on your own machine. It installs a
hash-pinned build of the llama.cpp engine for your hardware, prices a
curated catalog against that hardware, downloads models resumably into a
library every profile shares, and runs a loopback `llama-server` behind the
ordinary `llamacpp` provider. A local model is therefore selected, used as a
fallback entry and accounted like any other model (`billing_source =
"local"`, cost exactly 0).

The quickest way in is `prometheus model` and its **Local models (run on
this machine)** entry: it shows the catalog with the recommendation marked,
installs the engine, downloads the model you pick (Ctrl+C pauses; picking it
again resumes) and makes it the profile's model.

## The runtime: `[local_runtime]` in `daemon.toml`

The server is shared by every profile of a home, so its settings live in
`<PROMETHEUS_HOME>/daemon.toml`, never in a profile's `config.toml`:

```toml
[local_runtime]
enabled = false          # true: sessions start and supervise a server
backend = "auto"         # auto | cuda | metal | vulkan | hip | cpu
models_max = 4           # models resident at once (1-16)
port = 0                 # 0: a free loopback port at start
detect_ports = []        # ports probed for your own llama-server, besides 8080
idle_unload_secs = 900   # a model idle this long is unloaded, then reloaded
```

A profile picks a local model with `model.provider = "llamacpp"` and
`model.default = "<library id>"` (`prometheus local use <id>` writes both).
An unknown key or value, such as `backend = "rocm"`, is refused with exit
78 listing what is accepted.

`backend = "auto"` resolves from the hardware: CUDA for an NVIDIA GPU whose
driver supports CUDA 13 (Linux x86_64 and aarch64, Windows), Metal on macOS,
Vulkan for AMD and Intel GPUs (and older NVIDIA drivers) on Linux and
Windows x86_64, the CPU otherwise. HIP is used only when configured, on
x86_64. A discrete GPU is sized from its own memory minus what other
programs hold; an integrated GPU from system RAM.

## Where requests go

A `llamacpp` route without `[providers.llamacpp] base_url` finds its server
in this order:

1. a llama-server you run on `127.0.0.1:8080` or a `detect_ports` port
   (it must answer `/props` and `/v1/models`) — yours is always preferred;
2. the managed server another Prometheus process of this home recorded in
   `run/llamacpp.json`;
3. with `enabled = true`, a managed server started now.

Otherwise the route fails with `provider.local_runtime_down` ("the local
model server isn't running", exit 69) naming the remedies. It never falls
back to a remote provider unless your fallback chain names one, even when a
remote key is stored. An explicit `[providers.llamacpp] base_url` skips all
of this.

The managed server binds `127.0.0.1` in llama.cpp's router mode: every
library model is listed in `run/llamacpp-models.ini` with its window, at
most `models_max` are resident, and a model idle for `idle_unload_secs` is
unloaded and reloaded by its next request. The gateway daemon supervises
it when the daemon started it (`prometheus local start` hands it to a
running daemon); otherwise the process that started it does, through a
small helper that stops the server when that process exits. A crashed
server is restarted after 1, 2, 4, 8 and 16 seconds, then given up. Its
output goes to `logs/llamacpp.log`.

Nothing is downloaded when a session starts: the engine, the weights and the
catalog are read from disk, so a local model works offline. Downloads,
catalog changes and Hugging Face searches happen only through the commands
below.

## Commands

| Command | What it does |
|---|---|
| `prometheus local status [--json]` | engine version and backend, server state and port, resident models, GPU and RAM use, the library (partial downloads included) |
| `prometheus local install` | downloads and verifies the pinned engine for this machine's backend; an unsupported backend for your platform is refused with exit 69 before any request, and an archive that does not match its pin is deleted (exit 74) |
| `prometheus local update` | installs this build's engine pin and removes older builds the server does not use |
| `prometheus local catalog [--json]` | each model's `fit` (`fits_gpu`, `uses_system_ram`, `too_big` with the reason), starting and maximum window, quantisation (never below 4 bits), download size, speed estimate and the recommendation |
| `prometheus local pull <model>` | downloads a catalog model, or `owner/repo[:quant]` from Hugging Face; Ctrl+C pauses the job |
| `prometheus local jobs` | download jobs: `running`, `paused`, `interrupted`, `failed` |
| `prometheus local pause <job>` / `resume <job>` | pause at the next chunk (also a job another process runs) / continue, requesting only the missing bytes |
| `prometheus local search <query>` | Hugging Face GGUF repositories with downloads and a fit check per file (community, not validated) |
| `prometheus local add <path.gguf>` | links your file into the library without copying it |
| `prometheus local rm <model>` | removes the model's staged files; a linked file stays where it is |
| `prometheus local use <model>` | makes `llamacpp/<model>` the profile's model |
| `prometheus local start` / `stop` | runs the managed server (in the daemon when one runs, else in the foreground until Ctrl+C) / stops it |

On a fresh home nothing is installed or running, and a model that is not
in the library cannot be used:

```console
$ prometheus local jobs
no download jobs
$ prometheus local stop
no managed llama-server is running
$ prometheus local use qwen3.6-27b  # exits 66
<>no local model 'qwen3.6-27b' in the library
```

In a chat, `/local use <model>` switches the session to the model and
`/local use <model> --global` also saves it; `/local status` shows the
engine and server. The daemon serves the same operations as the `local.*`
RPC methods.

Downloads land in `cache/partials/` and become usable only once their size
and SHA-256 are verified: an interrupted download is listed as `partial`
and resumes where it stopped. Models live in `<PROMETHEUS_HOME>/models/`,
the engine in `<PROMETHEUS_HOME>/runtimes/llamacpp/<version>-<backend>/`.

## Known limits

`network.proxy` applies to every outbound request of a profile, loopback
included, so with a proxy set the model requests to `127.0.0.1` go through
it too; a proxy that cannot reach your machine's loopback breaks local
models (as it does other local servers).

## Not yet

Growing a model's window before compression and live GPU telemetry in the
status bars arrive with milestone M3; until then a model starts at the
window `local catalog` shows (at least 65536 tokens when it fits).
