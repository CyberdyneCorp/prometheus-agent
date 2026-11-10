//! On-device speech engines (spec: web-and-media-tools, Built-in
//! text-to-speech providers: `piper`, `kittentts`, `neutts`; Built-in
//! speech-to-text providers: `local` Whisper).
//!
//! Each engine runs as a worker process that loads its model once and
//! answers one JSON line per request on stdin/stdout. Workers are shared
//! by every session of the process: a surface holds a [`Lease`] while it
//! may speak or listen, and when the last lease is released the worker
//! stays loaded for the keep-warm time (`tts.keep_warm_secs`, or
//! `stt.local.unload_after_idle_secs` where `0` means never) before it is
//! stopped. The [`Launcher`] (the host's managed Python environment)
//! starts the workers; the drivers are the [`DRIVERS`] scripts.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Starts engine workers.
pub trait Launcher: Send + Sync {
    /// Whether `engine` can be started (its runtime is installed).
    fn available(&self, engine: &str) -> bool;
    /// The worker command of `engine`: the `package` installed, running
    /// the driver `script` (stdin/stdout piped by the caller).
    ///
    /// # Errors
    ///
    /// The runtime is missing.
    fn worker(
        &self,
        engine: &str,
        package: &str,
        script: &str,
    ) -> Result<tokio::process::Command, String>;
}

/// The engines and the Python package and driver of each.
pub const DRIVERS: &[(&str, &str, &str)] = &[
    ("piper", "piper-tts", PIPER),
    ("kittentts", "kittentts", KITTEN),
    ("neutts", "neuttsair", NEUTTS),
    ("whisper", "faster-whisper", WHISPER),
];

const PIPER: &str = r#"
import json, os, sys, wave
from pathlib import Path
from piper import PiperVoice
from piper.download_voices import download_voice
cache = Path(os.environ.get("PMT_ENGINE_DIR", "."))
voices = {}
def load(name):
    if name not in voices:
        model = cache / (name + ".onnx")
        if not model.exists():
            cache.mkdir(parents=True, exist_ok=True)
            download_voice(name, cache)
        voices[name] = PiperVoice.load(str(model))
    return voices[name]
for line in sys.stdin:
    req = json.loads(line)
    try:
        with wave.open(req["out"], "wb") as out:
            load(req["voice"]).synthesize_wav(req["text"], out)
        print(json.dumps({"ok": True}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)}), flush=True)
"#;

const KITTEN: &str = r#"
import json, sys
import soundfile
from kittentts import KittenTTS
models = {}
for line in sys.stdin:
    req = json.loads(line)
    try:
        name = req.get("model") or "KittenML/kitten-tts-nano-0.2"
        model = models.setdefault(name, KittenTTS(name))
        audio = model.generate(req["text"], voice=req.get("voice") or "expr-voice-2-f")
        soundfile.write(req["out"], audio, 24000)
        print(json.dumps({"ok": True}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)}), flush=True)
"#;

const NEUTTS: &str = r#"
import json, sys
import soundfile
from neuttsair.neutts import NeuTTSAir
tts = NeuTTSAir(backbone_repo="neuphonic/neutts-air-q4-gguf", codec_repo="neuphonic/neucodec")
refs = {}
for line in sys.stdin:
    req = json.loads(line)
    try:
        voice = req.get("voice")
        if not voice:
            raise ValueError("neutts needs tts.neutts.voice: a reference .wav with a .txt transcript beside it")
        if voice not in refs:
            text = open(voice.rsplit(".", 1)[0] + ".txt").read().strip()
            refs[voice] = (tts.encode_reference(voice), text)
        codes, text = refs[voice]
        soundfile.write(req["out"], tts.infer(req["text"], codes, text), 24000)
        print(json.dumps({"ok": True}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)}), flush=True)
"#;

const WHISPER: &str = r#"
import json, sys
from faster_whisper import WhisperModel
models = {}
for line in sys.stdin:
    req = json.loads(line)
    try:
        model = models.setdefault(req["model"], WhisperModel(req["model"]))
        segments, _ = model.transcribe(
            req["input"],
            language=req.get("language") or None,
            vad_filter=req.get("vad", True),
            vad_parameters={"min_silence_duration_ms": req.get("vad_min_silence_ms", 500)},
        )
        kept = [s.text for s in segments if not (s.no_speech_prob > 0.6 and s.avg_logprob < -1.0)]
        print(json.dumps({"ok": True, "text": "".join(kept).strip()}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)}), flush=True)
"#;

/// The package and driver of `engine`.
#[must_use]
pub fn driver(engine: &str) -> Option<(&'static str, &'static str)> {
    DRIVERS
        .iter()
        .find(|(name, ..)| *name == engine)
        .map(|(_, package, script)| (*package, *script))
}

struct Process {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One engine's worker and its leases.
struct Worker {
    process: tokio::sync::Mutex<Option<Process>>,
    leases: Mutex<(usize, u64)>,
}

/// The process-wide workers by engine.
static WORKERS: LazyLock<Mutex<HashMap<String, Arc<Worker>>>> = LazyLock::new(Mutex::default);

fn worker(engine: &str) -> Arc<Worker> {
    let mut map = WORKERS.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(map.entry(engine.to_owned()).or_insert_with(|| {
        Arc::new(Worker {
            process: tokio::sync::Mutex::new(None),
            leases: Mutex::new((0, 0)),
        })
    }))
}

/// Whether `engine`'s worker process is running now.
pub async fn loaded(engine: &str) -> bool {
    worker(engine).process.lock().await.is_some()
}

/// A surface's hold on an engine: while any lease is held the worker
/// stays loaded.
pub struct Lease {
    engine: String,
    keep_warm: Option<Duration>,
}

impl Lease {
    /// Holds `engine`; after the last release it stays loaded for
    /// `keep_warm` (`None`: until the process exits).
    #[must_use]
    pub fn acquire(engine: &str, keep_warm: Option<Duration>) -> Self {
        let worker = worker(engine);
        let mut leases = worker.leases.lock().unwrap_or_else(PoisonError::into_inner);
        leases.0 += 1;
        leases.1 += 1;
        Self {
            engine: engine.to_owned(),
            keep_warm,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let worker = worker(&self.engine);
        let generation = {
            let mut leases = worker.leases.lock().unwrap_or_else(PoisonError::into_inner);
            leases.0 = leases.0.saturating_sub(1);
            if leases.0 > 0 {
                return;
            }
            leases.1
        };
        let Some(keep_warm) = self.keep_warm else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        pmt_core::spawn::spawn_on(&runtime, async move {
            tokio::time::sleep(keep_warm).await;
            // A lease taken meanwhile (a new generation) keeps it loaded.
            let idle = {
                let leases = worker.leases.lock().unwrap_or_else(PoisonError::into_inner);
                leases.0 == 0 && leases.1 == generation
            };
            if idle {
                *worker.process.lock().await = None;
            }
        });
    }
}

/// Sends one request to `engine`'s worker, starting it when needed, and
/// returns its answer.
///
/// # Errors
///
/// The engine is unknown or not installed, the worker died, or it
/// answered an error.
pub async fn request(
    launcher: &dyn Launcher,
    engine: &str,
    request: &Value,
) -> Result<Value, String> {
    let (package, script) = driver(engine).ok_or_else(|| format!("unknown engine '{engine}'"))?;
    let worker = worker(engine);
    let mut slot = worker.process.lock().await;
    if slot.is_none() {
        let mut command = launcher.worker(engine, package, script)?;
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("{engine} could not start: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        *slot = Some(Process {
            _child: child,
            stdin,
            stdout,
        });
    }
    let process = slot.as_mut().ok_or("no worker")?;
    let answer = exchange(process, request).await;
    if answer.is_err() {
        // A broken worker is restarted by the next request.
        *slot = None;
    }
    let answer = answer?;
    match answer.get("error").and_then(Value::as_str) {
        Some(error) => Err(format!("{engine}: {error}")),
        None => Ok(answer),
    }
}

async fn exchange(process: &mut Process, request: &Value) -> Result<Value, String> {
    let mut line = request.to_string();
    line.push('\n');
    process
        .stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("the engine stopped: {e}"))?;
    let mut answer = String::new();
    let read = process
        .stdout
        .read_line(&mut answer)
        .await
        .map_err(|e| format!("the engine stopped: {e}"))?;
    if read == 0 {
        return Err("the engine exited".to_owned());
    }
    serde_json::from_str(&answer).map_err(|e| format!("unreadable engine answer: {e}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A worker that answers every line with `{"ok":true,"text":"heard"}`
    /// and touches the requested output file.
    struct Fake;

    impl Launcher for Fake {
        fn available(&self, _: &str) -> bool {
            true
        }

        fn worker(&self, _: &str, _: &str, _: &str) -> Result<tokio::process::Command, String> {
            let mut c = tokio::process::Command::new("sh");
            c.args([
                "-c",
                r#"while read -r line; do echo '{"ok":true,"text":"heard"}'; done"#,
            ]);
            Ok(c)
        }
    }

    #[tokio::test]
    async fn workers_stay_warm_until_the_last_lease_and_keep_warm_pass() {
        let first = Lease::acquire("kittentts", Some(Duration::from_millis(200)));
        let second = Lease::acquire("kittentts", Some(Duration::from_millis(200)));
        let answer = request(&Fake, "kittentts", &serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(answer["text"], "heard");
        drop(first);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(loaded("kittentts").await, "a lease is still held");
        drop(second);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(loaded("kittentts").await, "kept warm");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!loaded("kittentts").await, "unloaded after keep-warm");
    }
}
