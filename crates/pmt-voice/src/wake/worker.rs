//! Wake engines as worker processes: the detection libraries are Python
//! packages (openWakeWord, sherpa-onnx, Porcupine), so each engine runs as
//! a worker of the host's managed Python environment that loads its model
//! once and answers one JSON line per frame on stdin/stdout. Nothing
//! leaves the machine: the worker only ever sees the frames written to
//! it, and the Porcupine access key arrives on stdin, never in argv or
//! the environment.
//!
//! Protocol: the first line is the [`Init`]; the worker answers
//! `{"ready": true, "frame": <samples>}` or `{"error": "..."}`. Then each
//! `{"pcm": "<base64 s16le>"}` gets `{"score": <0-1>}` (scored engines,
//! confirmed here over `wake.confirmation_frames`), `{"match": {"phrase":
//! ..., "profile": ...}}` or `{}`; `{"reset": true}` gets `{"ok": true}`.

use base64::Engine as _;
use pmt_config::schema::WakeEngine as Kind;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::engine::{Confirm, Detection, WakeEngine};
use crate::device::BoxFuture;

/// One phrase the worker listens for and the profile it wakes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Phrase {
    /// The phrase.
    pub phrase: String,
    /// The profile it wakes.
    pub profile: String,
}

/// What a worker is started with.
#[derive(Debug, Clone, Serialize)]
pub struct Init {
    /// `openwakeword`, `sherpa` or `porcupine`.
    pub engine: &'static str,
    /// The model (a built-in name or an absolute path): the
    /// `openwakeword` model or the `porcupine` keyword.
    pub model: String,
    /// The phrases `sherpa` spots.
    pub phrases: Vec<Phrase>,
    /// The engine's threshold or sensitivity ([`super::engine::Thresholds`]).
    pub threshold: f64,
    /// sherpa's model directory (downloaded there once when missing).
    pub model_dir: String,
    /// Porcupine's access key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key: Option<String>,
}

/// A running engine worker.
pub struct WorkerEngine {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    frame: usize,
    /// `openwakeword`: scores confirmed over consecutive frames.
    confirm: Option<Confirm>,
    /// The phrase a scored engine reports.
    phrase: String,
}

impl WorkerEngine {
    /// Starts `command` (stdin and stdout are piped here) and loads the
    /// engine `init` describes; `confirmation_frames` applies to scored
    /// engines.
    ///
    /// # Errors
    ///
    /// The worker did not start or could not load the engine (its error).
    pub async fn start(
        mut command: Command,
        init: &Init,
        label: &str,
        confirmation_frames: u32,
    ) -> Result<Self, String> {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("the {} worker did not start: {e}", init.engine))?;
        let stdin = child.stdin.take().ok_or("no worker stdin")?;
        let stdout = child.stdout.take().ok_or("no worker stdout")?;
        let mut engine = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            frame: 1_280,
            confirm: None,
            phrase: label.to_owned(),
        };
        let line = serde_json::to_string(init).map_err(|e| e.to_string())?;
        let ready = engine.ask(&line).await?;
        if let Some(error) = ready["error"].as_str() {
            return Err(error.to_owned());
        }
        engine.frame = ready["frame"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(1_280);
        if init.engine == Kind::Openwakeword.as_str() {
            engine.confirm = Some(Confirm::new(init.threshold, confirmation_frames));
        }
        Ok(engine)
    }

    /// Writes one line and reads the answer.
    async fn ask(&mut self, line: &str) -> Result<Value, String> {
        let gone = |e: std::io::Error| format!("the wake worker stopped: {e}");
        self.stdin.write_all(line.as_bytes()).await.map_err(gone)?;
        self.stdin.write_all(b"\n").await.map_err(gone)?;
        self.stdin.flush().await.map_err(gone)?;
        let Some(answer) = self.lines.next_line().await.map_err(gone)? else {
            let status = self.child.try_wait().ok().flatten();
            return Err(format!("the wake worker exited ({status:?})"));
        };
        serde_json::from_str(&answer).map_err(|e| format!("the wake worker answered nonsense: {e}"))
    }
}

fn pcm(frame: &[i16]) -> String {
    let bytes: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl WakeEngine for WorkerEngine {
    fn frame_samples(&self) -> usize {
        self.frame
    }

    fn feed<'a>(
        &'a mut self,
        frame: &'a [i16],
    ) -> BoxFuture<'a, Result<Option<Detection>, String>> {
        Box::pin(async move {
            let line = format!("{{\"pcm\":\"{}\"}}", pcm(frame));
            let answer = self.ask(&line).await?;
            if let Some(error) = answer["error"].as_str() {
                return Err(error.to_owned());
            }
            if let Some(score) = answer["score"].as_f64() {
                let fired = self.confirm.as_mut().is_some_and(|c| c.score(score));
                return Ok(fired.then(|| Detection {
                    phrase: self.phrase.clone(),
                    profile: None,
                }));
            }
            Ok(answer["match"]["phrase"].as_str().map(|phrase| Detection {
                phrase: phrase.to_lowercase(),
                profile: answer["match"]["profile"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned),
            }))
        })
    }

    fn reset(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(confirm) = &mut self.confirm {
                confirm.reset();
            }
            if let Err(why) = self.ask("{\"reset\":true}").await {
                tracing::warn!(%why, "the wake worker did not reset");
            }
        })
    }
}

/// Each engine's pinned Python package and driver. Exact releases: the
/// worker runs outside the session sandbox, so a newer upload never runs
/// unreviewed.
pub const DRIVERS: &[(Kind, &str, &str)] = &[
    (Kind::Openwakeword, "openwakeword==0.6.0", OPENWAKEWORD),
    (Kind::Sherpa, "sherpa-onnx==1.12.0", SHERPA),
    (Kind::Porcupine, "pvporcupine==3.0.5", PORCUPINE),
];

/// The package and driver of `kind`.
#[must_use]
pub fn driver(kind: Kind) -> Option<(&'static str, &'static str)> {
    DRIVERS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, package, script)| (*package, *script))
}

/// How to make `kind` runnable: the workers run on the managed `uv`
/// runtime, which fetches the pinned package on first use.
#[must_use]
pub fn install_hint(kind: Kind) -> String {
    driver(kind).map_or_else(String::new, |(package, _)| {
        format!("run 'prometheus runtimes install uv' ({package} is fetched on first use)")
    })
}

const PROLOGUE: &str = r#"
import base64, json, sys
def say(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
init = json.loads(sys.stdin.readline())
"#;

const OPENWAKEWORD: &str = r#"
import numpy as np
try:
    import openwakeword
    from openwakeword.model import Model
    ref = init["model"]
    if not ref.endswith((".onnx", ".tflite")):
        openwakeword.utils.download_models([ref])
    model = Model(wakeword_models=[ref], inference_framework="onnx")
except Exception as e:
    say({"error": f"openwakeword could not load {init['model']}: {e}"}); sys.exit(1)
say({"ready": True, "frame": 1280})
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reset"):
        model.reset(); say({"ok": True}); continue
    frame = np.frombuffer(base64.b64decode(msg["pcm"]), dtype=np.int16)
    scores = model.predict(frame)
    say({"score": float(max(scores.values())) if scores else 0.0})
"#;

const SHERPA: &str = r#"
import array, os, tarfile, tempfile, urllib.request
URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2"
NAME = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
try:
    import sherpa_onnx
    from sherpa_onnx import text2token
    root = init["model_dir"]
    d = root if os.path.exists(os.path.join(root, "tokens.txt")) else os.path.join(root, NAME)
    if not os.path.exists(os.path.join(d, "tokens.txt")):
        os.makedirs(root, exist_ok=True)
        archive = os.path.join(root, NAME + ".tar.bz2")
        urllib.request.urlretrieve(URL, archive)
        with tarfile.open(archive, "r:bz2") as tf:
            tf.extractall(root, filter="data")
        os.unlink(archive)
    phrases = init["phrases"]
    tokens = text2token([p["phrase"].upper() for p in phrases], tokens=os.path.join(d, "tokens.txt"),
                        tokens_type="bpe", bpe_model=os.path.join(d, "bpe.model"))
    by_label = {}
    kw = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
    for p, toks in zip(phrases, tokens):
        label = p["phrase"].upper().replace(" ", "_")
        by_label[label] = p
        kw.write(" ".join(toks) + f" @{label}\n")
    kw.close()
    def part(name):
        hits = sorted(f for f in os.listdir(d) if f.startswith(name + "-") and f.endswith(".onnx") and not f.endswith("int8.onnx"))
        return os.path.join(d, hits[0])
    spotter = sherpa_onnx.KeywordSpotter(tokens=os.path.join(d, "tokens.txt"), encoder=part("encoder"),
        decoder=part("decoder"), joiner=part("joiner"), keywords_file=kw.name,
        keywords_threshold=init["threshold"], num_threads=1)
    stream = spotter.create_stream()
    os.unlink(kw.name)
except Exception as e:
    say({"error": f"sherpa-onnx could not load its keyword model: {e}"}); sys.exit(1)
say({"ready": True, "frame": 1280})
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reset"):
        stream = spotter.create_stream(); say({"ok": True}); continue
    frame = array.array("h", base64.b64decode(msg["pcm"]))
    stream.accept_waveform(16000, [s / 32768.0 for s in frame])
    found = None
    while spotter.is_ready(stream):
        spotter.decode_stream(stream)
        result = spotter.get_result(stream)
        if result:
            found = by_label.get(str(result))
            spotter.reset_stream(stream)
    say({"match": found} if found else {})
"#;

const PORCUPINE: &str = r#"
import os, struct
try:
    import pvporcupine
    ref = init["model"]
    path = os.sep in ref or ref.endswith(".ppn")
    args = {"access_key": init["access_key"], "sensitivities": [init["threshold"]]}
    args["keyword_paths" if path else "keywords"] = [ref]
    porcupine = pvporcupine.create(**args)
except Exception as e:
    say({"error": f"porcupine could not load {init['model']}: {e}"}); sys.exit(1)
say({"ready": True, "frame": porcupine.frame_length})
phrase = init["phrases"][0] if init["phrases"] else {"phrase": init["model"], "profile": ""}
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reset"):
        say({"ok": True}); continue
    raw = base64.b64decode(msg["pcm"])
    frame = struct.unpack("<%dh" % (len(raw) // 2), raw)
    say({"match": phrase} if porcupine.process(frame) >= 0 else {})
"#;

/// The full driver script of `kind` (the shared prologue first).
#[must_use]
pub fn script(kind: Kind) -> Option<String> {
    driver(kind).map(|(_, body)| format!("{PROLOGUE}{body}"))
}

/// The phrases one `sherpa` listener spots (spec: voice-mode, Wake word
/// profile routing): the listening profile's own, then, with
/// `wake.profile_routing`, each other profile with wake enabled under its
/// `wake.phrase`, or `hey <profile>` when it kept the default phrase.
#[must_use]
pub fn enroll(
    own: (&str, &pmt_config::schema::WakeConfig),
    others: &[(String, pmt_config::schema::WakeConfig)],
) -> Vec<Phrase> {
    let (profile, config) = own;
    let mut phrases = vec![Phrase {
        phrase: config.phrase.to_lowercase(),
        profile: profile.to_owned(),
    }];
    if !config.profile_routing {
        return phrases;
    }
    let default = pmt_config::schema::WakeConfig::default().phrase;
    for (name, other) in others {
        if name == profile || !other.enabled {
            continue;
        }
        let phrase = if other.phrase == default {
            format!("hey {name}")
        } else {
            other.phrase.to_lowercase()
        };
        if !phrases.iter().any(|p| p.phrase == phrase) {
            phrases.push(Phrase {
                phrase,
                profile: name.clone(),
            });
        }
    }
    phrases
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::WakeConfig;

    use super::*;

    fn enabled() -> WakeConfig {
        WakeConfig {
            enabled: true,
            ..WakeConfig::default()
        }
    }

    #[test]
    fn one_listener_enrolls_every_profile_with_wake_enabled() {
        let others = vec![
            ("coder".to_owned(), enabled()),
            ("off".to_owned(), WakeConfig::default()),
            (
                "home".to_owned(),
                WakeConfig {
                    phrase: "Computer".to_owned(),
                    ..enabled()
                },
            ),
        ];
        let phrases = enroll(("default", &enabled()), &others);
        let pairs: Vec<(&str, &str)> = phrases
            .iter()
            .map(|p| (p.phrase.as_str(), p.profile.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("hey prometheus", "default"),
                ("hey coder", "coder"),
                ("computer", "home")
            ]
        );
        let own_only = WakeConfig {
            profile_routing: false,
            ..enabled()
        };
        assert_eq!(enroll(("default", &own_only), &others).len(), 1);
    }

    #[test]
    fn every_engine_has_a_pinned_driver() {
        for kind in [Kind::Openwakeword, Kind::Sherpa, Kind::Porcupine] {
            let (package, _) = driver(kind).unwrap();
            assert!(package.contains("=="), "{package} is pinned");
            assert!(script(kind).unwrap().contains("say({\"ready\": True"));
        }
    }
}
