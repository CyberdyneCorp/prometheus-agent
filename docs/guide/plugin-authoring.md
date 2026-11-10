# Writing a plugin

This guide builds a WebAssembly plugin from scratch: a `tool` plugin that
gives the agent a forecast tool. The finished plugin lives in the
repository at `plugins/examples/weather/`. On every run CI builds it with
`cargo component` (`cargo xtask plugin-samples --check`), and a test builds
the same source into a component, then validates, packs, installs and loads
it with its `plugin.toml`, so the code below is known to work. [Plugins](plugins.md) covers installing and managing plugins as a
user.

## What a plugin is

A plugin is a directory with two files:

- `plugin.toml`, the manifest: id, version, the WIT worlds it implements
  and the capabilities it asks for;
- `plugin.wasm`, a WebAssembly *component* implementing those worlds.

The component runs inside Prometheus with no ambient authority: no files,
network, clock or environment variables. Everything it reaches goes
through the host interfaces in `wit/host/host.wit`, and each of those
checks the capabilities the user approved. Asking for a host the manifest
did not declare fails with `capability_denied: network <host>`; nothing is
opened.

## Tools you need

- Rust with the `wasm32-unknown-unknown` target
  (`rustup target add wasm32-unknown-unknown`);
- `cargo component` 0.21 (`cargo install --locked cargo-component@0.21.1`).

Any language with a component toolchain works; the WIT files in `wit/` are
the contract.

## The project

```text
weather/
├── Cargo.toml
├── plugin.toml
└── src/lib.rs
```

`Cargo.toml` names the world to build against. The paths point at the
repository's `wit/` directory; outside the repository, copy `wit/host` and
the world you implement next to your crate.

```toml
[package]
name = "example-weather"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen-rt = { version = "0.41", features = ["bitflags"] }

[package.metadata.component]
package = "prometheus-examples:weather"

[package.metadata.component.target]
path = "../../../wit/tool"
world = "tool"

[package.metadata.component.target.dependencies]
"prometheus:host" = { path = "../../../wit/host" }
```

`plugin.toml` declares the plugin. The id must match the directory name
once installed; `worlds` lists every world the component exports, with the
version it was built against; `[capabilities]` asks for exactly what the
code uses, here one host:

```toml
manifest_version = 1
id = "weather"
version = "1.0.0"
kind = "wasm"
description = "Current weather for a city, from wttr.in."
license = "MIT"
requires_prometheus = ">=0.1"
worlds = ["tool@1.0"]

[capabilities]
network = ["wttr.in"]
```

## The code

`cargo component` generates `src/bindings.rs` from the WIT on every build.
The `tool` world asks for two functions: `tools()`, read once when the
plugin loads, and `call(name, arguments)`, run for each tool call with the
model's arguments as JSON text (already validated against the schema).

```rust,ignore
#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::tool::plugin::{Guest, ToolSpec};
use bindings::prometheus::host::{config, http, state};

struct Weather;

impl Guest for Weather {
    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "forecast".to_owned(),
            description: "Current weather for a city.".to_owned(),
            parameters: r#"{"type":"object","properties":{"city":{"type":"string"}}}"#.to_owned(),
        }]
    }

    fn call(name: String, arguments: String) -> Result<String, String> {
        // ...read `city` from the arguments or the `city` setting, then:
        let request = http::Request {
            method: "GET".to_owned(),
            url: format!("https://wttr.in/{city}?format=3"),
            headers: Vec::new(),
            body: None,
        };
        let response = http::send(&request).map_err(|e| format!("{e:?}"))?;
        let text = String::from_utf8_lossy(&response.body).trim().to_owned();
        let _ = state::set(&format!("last:{city}"), text.as_bytes());
        Ok(text)
    }
}

bindings::export!(Weather with_types_in bindings);
```

The full example adds a fallback: when wttr.in cannot be reached it answers
the last forecast it stored with `state`, which lives under the profile's
`plugin-data/weather/` and survives restarts. `config::get("city")` reads
the plugin's own setting, `plugins.entries.weather.city` in `config.toml`.
`Ok(text)` is the tool result the model sees; `Err(text)` is an error result
the model sees, and the turn goes on.

Build it:

```sh
cargo component build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/example_weather.wasm plugin.wasm
```

The file is named after the crate (`example-weather` here, with `_` for
`-`) and lands in the `target/` directory of the Cargo workspace the crate
belongs to. For a plugin in a project of its own that is the project's
`target/`, as above. The in-repository example is a member of the
`plugins/` workspace, so there it lands in
`plugins/target/wasm32-unknown-unknown/release/example_weather.wasm`.

## Check, package, install

`validate` checks what loading would: the manifest, that the component
compiles and exports every world it declares, and the capability
declarations. It exits 65 on any problem.

```console
$ prometheus plugins validate ./no-such-plugin # exits 65
error: <...>
```

```sh
prometheus plugins validate .
prometheus plugins pack . --out weather-1.0.0.pmtpkg   # prints the package digest
prometheus plugins install ./weather-1.0.0.pmtpkg --allow-unsigned --enable
```

A package you built yourself carries no signature, so installing it needs
`--allow-unsigned` (with `plugins.signature_policy = "require"`, the
default); the flag is kept on the install record. `--enable` shows what the
plugin asks for and asks you to approve it (`--grant all` approves without
a terminal). The tool joins sessions started from then on, in a toolset
named after the plugin.

To publish, sign the package with a minisign key and give the catalog your
public key:

```sh
minisign -G -W -p publisher.pub -s publisher.key   # an unencrypted key pair
prometheus plugins sign weather-1.0.0.pmtpkg --key publisher.key
```

`sign` writes `weather-1.0.0.pmtpkg.minisig`, a signature over the package
digest. A catalog entry pins that digest (`ref = "sha256:..."`), the
package URL and your `publisher_key`; `prometheus plugins install weather`
then refuses anything that does not match.

## Changing it

Editing an installed plugin's files makes it `modified after install` and
it stops loading. Rebuild, pack and `prometheus plugins update weather`
instead: an update that asks for nothing new keeps its approval; one that
asks for more keeps the old version running until you approve the
difference (`prometheus plugins diff-grants weather` shows it). A
directory you copy into the profile's `plugins/` by hand stays `unadopted`
until `prometheus plugins adopt <dir>`.

`prometheus plugins reload` swaps the running plugin for its current files
without a restart: hooks, slash commands and event subscribers change at
once, while tools, system-prompt sections and skills reach new sessions
only (the current session's cached prompt stays as it was). `/plugins
reload --now` applies tools and sections to the current session too;
skills always wait for a new session.

## Other worlds

`plugins/samples/` holds a minimal plugin for every world. Beyond tools, the
`hook` world is the one most plugins use:

- `hooks()` names the hooks to receive (each also listed in the manifest's
  `hooks` capability; an unknown name fails the load) and `on-hook` answers
  each firing: `none`, `context` (for `pre_llm_call`, text added to the
  current user message), or for `pre_tool_call` `modify`, `block` or
  `approve`.
- Its `extensions` interface (hook world 1.1) adds system-prompt sections
  (`sections()` registers ids, `render-section` is asked once per session,
  within the `prompt_section` capability's size and 4000 characters),
  slash commands (`commands()`, each listed in the `commands` capability;
  a core command's name is refused) and event delivery: a plugin calls
  `events.subscribe("other:*")` and receives matching events through
  `on-event`; `events.emit("updated", payload)` publishes
  `weather:updated`.

- Its `surfaces` interface (hook world 1.2) holds the stream for
  `transform_llm_output` (`hold-stream`), registers `prometheus plugins
  run` commands (`cli-commands`, `run-cli` with argv and the terminal's
  size; each name in the `cli_commands` capability) and approval
  transports (`transports`, `present-approval`, which must answer with the
  request's `id`, `digest` and one of its `choices`).
- Hook world 1.3 imports `console` (`write-stdout`, `write-stderr`): a
  `run-cli` command writes through it to stream its output to the
  invoking terminal as it goes; the `cli-output` it returns is printed
  after. Outside `run-cli` the writes are dropped.
- Transform hooks answer with `context`: the new tool result
  (`transform_tool_result`) or reply (`transform_llm_output`).
  `transform_api_error_classification` and `pre_verify` answer `modify`
  with their JSON object.

Tool and hook plugins also import `prometheus:messaging`:
`conversation.post`/`edit` (a message in the calling session's chat),
`inject.inject` (with `inject_messages`) and `platform.add-reaction` /
`set-thread-title` (with `platform_actions`).

Tool plugins (world 1.2) also import `prometheus:subagents/lifecycle`, with
the `subagents` capability: `launch(task)` starts a subagent of the calling
session (`task` is one [`delegate_task`](../reference/tools.md#delegate_task) task object as JSON, checked and
narrowed exactly like a spawn by the model) and answers a signed handle;
`status`, `wait(handle, timeout-ms)`, `steer`, `cancel` and `result` follow
it. A handle works only for the conversation it was issued in; a forged,
foreign or expired one answers `unknown-handle`. Results stay readable for
an hour after the child ends.

Terminal-backend plugins (world 1.2) also import
`prometheus:credentials/handles`, with the `credentials` capability
(`credentials = ["modal"]`): `describe(handle)` answers the kind, origin
and billing source of a credential the host holds, and `send(handle,
request)` sends an HTTP request with that credential attached by the host,
so the plugin never sees the secret. An ungranted or unknown handle, an
unavailable credential, and a URL that is not `https` on one of the
credential's own hosts are refused before anything is sent; the request
also needs the `network` grant like any `http.send`.

A `delegation-router` plugin routes [`delegate_task`](../reference/tools.md#delegate_task) tasks when
`delegation.router` names it. `route(task)` receives the task as JSON and
answers `{"model": ..., "profile": ..., "backend": ...}` (every field
optional), or an error that refuses only that task. Its name and the task
parameters it adds are declared in `plugin.toml`:

```toml
worlds = ["delegation-router@1.0"]

[delegation_router]
name = "tiers"                       # the plugin id when absent

[delegation_router.params]
tier = { type = "string", enum = ["fast", "deep"] }
```

A `provider` plugin registers named providers (image generation, web
search, text-to-speech...) that config selects by name, such as
`web.search_backend = "<name>"`. Its `models` interface (provider world
1.1) registers model providers: `descriptors()` answers JSON descriptors
(`id`, `wire_format`, `base_url`, `secret_names`, `key_hosts`, ...), and
`headers`/`prepare-request` shape each request the host sends.

A model provider that signs in with OAuth declares the sign-in as data:
an `oauth` object in its descriptor with `client_id`, `authorize_url`,
`token_url` and optionally `scopes`, `allowed_hosts` (default: the
authorize URL's host and its subdomains), `redirect_port` (0 picks a free
one), `redirect_path` and `authorize_params`. `prometheus auth add <id>
--type oauth` then runs the browser PKCE flow in the host, which keeps the
verifier, `state` and every token in the profile's secrets store and
signs each request itself. `prepare-request` receives only an opaque
handle naming the credential in the body field `prometheus_credential`
(`subscription:<id>/<label>`), removed before the request is sent. A
Python model-provider plugin declares the same sign-in through Hermes's
`hermes_cli.auth_oauth_pkce_plugin` (`OAuthPKCEConfig`,
`pkce_auth_handler`, `pkce_refresh_credential`); a `token_request` broker
is refused at load, since it would receive the verifier.

A descriptor may instead name an agent CLI with `command`: the provider
becomes `external_process`, but the host launches only what the user
configures in `providers.<id>.command` (the plugin's value is shown as the
suggestion).

A plugin that needs another declares `requires_plugins = ["other"]`; it
loads after it, and not at all when `other` is missing, disabled or failed.

A plugin can add [scheduled-job blueprints](cron.md#blueprints) as data:
each `[[automation_blueprints]]` table (`key`, `title`, `description`,
`category`, `schedule`, `prompt`, `slots`) joins the catalog as
`<plugin>:<key>` while the plugin is enabled. A Python plugin calls
`ctx.register_automation_blueprint(key, title=..., schedule=..., prompt=...,
slots=[...])` instead.

A `memory-provider` plugin exports `plugin` (`name`, `is-available`,
`initialize`, `tool-schemas`) and optionally `hooks` (`prefetch`,
`sync-turn`, `on-memory-write`, `on-pre-compress`, `config-schema` and the
rest; without `hooks` each is a no-op). The host bounds, isolates, redacts
and queues every call; see the Memory guide. A `context-engine` plugin
exports `initialize`, `compact`, `select`, `tool-schemas` and
`handle-tool-call`.

## Limits worth designing for

- Each call has `plugins.wasm.call_timeout_ms` (default 30 s) of CPU time;
  a loop that never returns is stopped with `plugin '<id>' exceeded call
  timeout`. Three traps within a minute suspend the plugin until it is
  reloaded.
- Loading (instantiation plus `tools()`, `hooks()` and the other
  registration calls) has `plugins.load_timeout_ms` (default 10 s).
- Memory is capped by `plugins.wasm.max_memory_bytes` (256 MiB).
- Hook callbacks have `plugins.hook_timeout_ms`; a slow one is skipped for
  60 seconds.
- `llm.complete` runs on the session's model and is charged to the session
  under `plugin.<id>`; naming another model needs `allow_model_override`.
