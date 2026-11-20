//! A home whose model is a managed local model (`llamacpp/qwen-x`) on the
//! `pmt-llama-stub` engine, with a 24 GiB GPU hardware fixture (spec:
//! local-models): its daemon must start with
//! `PMT_TEST_LOCAL_FIXTURES=<home>/fixtures` ([`FIXTURES_VAR`],
//! [`fixtures`]).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{append_config, ok, run, temp_home};

/// The variable naming the hardware fixture directory.
pub const FIXTURES_VAR: &str = "PMT_TEST_LOCAL_FIXTURES";

/// The hardware fixture directory of [`local_home`].
pub fn fixtures(home: &Path) -> PathBuf {
    home.join("fixtures")
}

/// A GiB in bytes.
pub const GIB: u64 = 1 << 30;

/// A home on `llamacpp/qwen-x` (launched at 65536, native 262144, which
/// the server's model listing reports) with a 24 GiB GPU fixture.
pub fn local_home() -> tempfile::TempDir {
    let home = temp_home();
    let h = home.path();
    ok(&run(h, &["config", "set", "model.provider", "llamacpp"]));
    ok(&run(h, &["config", "set", "model.default", "qwen-x"]));
    // The stub reports 12 prompt tokens whatever it is sent: a low trigger
    // keeps the measured conversation clearly past it.
    append_config(h, "\n[compression]\nthreshold = 0.5\n");
    let version = pmt_local::engine::EngineIndex::embedded().version;
    let engine = h.join(format!("runtimes/llamacpp/{version}-cpu"));
    std::fs::create_dir_all(&engine).unwrap();
    let record = json!({
        "version": version, "backend": "cpu", "platform": "test",
        "exe": pmt_local::testkit::stub(), "library_dirs": [], "archives": [], "installed_at": "",
    });
    std::fs::write(
        engine.join(pmt_local::engine::RECORD_FILE),
        record.to_string(),
    )
    .unwrap();
    let model = h.join("models/qwen-x");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(
        model.join("w.gguf"),
        pmt_local::gguf::fixture("qwen3", 262_144, 4, 2, 64),
    )
    .unwrap();
    let record = json!({
        "id": "qwen-x", "source": "catalog", "context": 65536, "added_at": "",
        "files": [{"path": model.join("w.gguf"), "role": "weights", "size": 4}],
    });
    std::fs::write(model.join("model.json"), record.to_string()).unwrap();
    let fixtures = fixtures(h);
    std::fs::create_dir_all(&fixtures).unwrap();
    let hardware = json!({"platform": "linux-x64", "ram_total": 32 * GIB, "ram_available": 16 * GIB,
        "gpus": [{"vendor": "nvidia", "name": "NVIDIA GeForce RTX 4090", "memory_total": 24 * GIB,
            "memory_used": 2 * GIB, "cuda": 13, "utilization_percent": 37.0}]});
    std::fs::write(fixtures.join("hardware.json"), hardware.to_string()).unwrap();
    std::fs::write(h.join("daemon.toml"), "[local_runtime]\nenabled = true\n").unwrap();
    home
}
