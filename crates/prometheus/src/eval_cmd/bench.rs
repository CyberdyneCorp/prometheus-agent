//! Where `bench:<name>` comes from (spec: replay-and-evaluation,
//! Benchmark adapter protocol): a subprocess adapter
//! (`<profile>/evals/bench/<name>/adapter.toml`) first, else an enabled
//! WASM plugin `<name>` exporting the `bench` world, else an enabled
//! Python-bridge plugin that registered adapter `<name>`. The two plugin
//! transports serve the call's directory and the host's request answers
//! through the same [`BenchPort`], so a plugin reaches nothing else and a
//! pushed score is refused with -32002 whatever the transport.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_eval::bench::{Adapter, CALL_TIMEOUT, Call, Files, Host, Phase, Transport};
use pmt_plugins::host::ports::BenchPort;
use pmt_plugins::host::{CallInfo, Instance};
use pmt_pybridge::host::BridgeHost;
use pmt_pybridge::supervisor::Supervisor;
use serde_json::{Value, json};

/// One call's directory and host answers, as the plugin imports see them.
struct Port {
    files: Files,
    host: Host,
}

impl Port {
    fn of(call: &Call) -> Arc<dyn BenchPort> {
        Arc::new(Self {
            files: call.files.clone(),
            host: call.host.clone(),
        })
    }
}

impl BenchPort for Port {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files.read(path)
    }

    fn write(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        self.files.write(path, contents)
    }

    fn list(&self) -> Result<Vec<String>, String> {
        self.files.list()
    }

    fn request(&self, method: &str, _params: &str) -> (i64, String) {
        let error = self.host.answer(method);
        (error.code, error.message)
    }
}

/// A WASM plugin of the `bench` world.
struct Wasm(Arc<Instance>);

impl Transport for Wasm {
    fn call(&mut self, method: &str, params: &Value, call: &Call) -> Result<Value, String> {
        let info = CallInfo {
            surface: "eval".to_owned(),
            ..CallInfo::default()
        };
        match self
            .0
            .bench_call(&info, Port::of(call), method, &params.to_string())
        {
            Ok(Ok(text)) => {
                serde_json::from_str(&text).map_err(|e| format!("{method}: not JSON: {e}"))
            }
            Ok(Err((code, message))) => Err(format!(
                "{method}: {}",
                json!({"code": code, "message": message})
            )),
            Err(err) => Err(err.message().to_owned()),
        }
    }
}

/// A Python-bridge plugin's registered adapter.
struct Bridged {
    supervisor: Arc<Supervisor>,
    host: Arc<BridgeHost>,
    adapter: String,
}

impl Transport for Bridged {
    fn call(&mut self, method: &str, params: &Value, call: &Call) -> Result<Value, String> {
        self.host.set_bench(Some(Port::of(call)));
        let answer = self.supervisor.call(
            "bench.invoke",
            &json!({"adapter": self.adapter, "method": method, "params": params}),
            CALL_TIMEOUT,
        );
        self.host.set_bench(None);
        answer.map_err(|e| format!("{method}: {}", e.message()))
    }
}

/// Opens adapter `name` of `scope` for `phase`.
///
/// # Errors
///
/// `eval.suite_not_found` (66) when no adapter of that name exists;
/// `eval.bench_failed` (69) when it does not start.
pub fn open(scope: &ProfileScope, name: &str, phase: Phase) -> Result<Adapter, Error> {
    if pmt_eval::bench::has_subprocess(scope.dir(), name) {
        return Adapter::open(scope.dir(), name, phase);
    }
    let plugins = crate::plugins_host::for_session(scope, scope.dir());
    let loaded = plugins.current();
    if let Some(plugin) = loaded
        .plugins
        .iter()
        .find(|p| p.manifest.id == name && p.instance.implements("bench"))
    {
        let dir = plugin.dir.clone().unwrap_or_default();
        let transport = Wasm(Arc::clone(&plugin.instance));
        return Ok(Adapter::new(name, dir, phase, Box::new(transport)));
    }
    if let Some((supervisor, host)) = crate::plugins_host::bridge::bench_adapter(scope, name) {
        let dir = scope.dir().join("plugins").join(supervisor.plugin());
        let transport = Bridged {
            supervisor,
            host,
            adapter: name.to_owned(),
        };
        return Ok(Adapter::new(name, dir, phase, Box::new(transport)));
    }
    Err(Error::new(
        &codes::EVAL_SUITE_NOT_FOUND,
        format!(
            "no benchmark adapter '{name}' ({} is missing, and no enabled plugin provides it)",
            pmt_eval::bench::dir(scope.dir(), name)
                .join("adapter.toml")
                .display()
        ),
    ))
}
