//! `text_to_speech` (spec: web-and-media-tools, `text_to_speech` tool and
//! delivery).
//!
//! The text is spoken whole (chunked under the provider's cap and joined)
//! into `output_path` (inside the workspace or `<profile>/cache/audio/`)
//! or a new file under `<profile>/cache/audio/`. The result is
//! `MEDIA:<path>` with `voice_compatible`; on a voice-bubble platform the
//! line carries `[[as_voice]]` so the reply sends a voice message. With
//! `elevenlabs` the tool also takes a `voice` and lists the account's
//! voices.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::Config;
use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::result::ToolResult;
use pmt_tools::usage;
use serde_json::{Value, json};

use super::tts::Speak;
use super::{VOICE_BUBBLES, audio_dir};
use crate::{MediaSession, MediaSessions};

/// The tool's toolset.
pub const TOOLSET: &str = "tts";
/// Bounds of `speed`.
pub const SPEED: (f64, f64) = (0.25, 4.0);

struct Handler(Arc<MediaSessions>);

/// `text_to_speech`.
#[must_use]
pub fn registered(sessions: &Arc<MediaSessions>) -> RegisteredTool {
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(sessions))),
    )
    .with_parameters(Arc::new(parameters))
    .with_probe(Arc::new(pmt_tools::probe::FnProbe::new(
        "tts.credential",
        crate::probe::speech,
    )))
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "text_to_speech",
        TOOLSET,
        "Speak text aloud with the configured voice into an audio file. Long text is \
         split and joined, never cut. The result names the file as MEDIA:<path>; put that \
         line in your reply to send the audio (as a voice message where the platform has \
         them).",
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "What to say."},
                "output_path": {"type": "string", "description": "Where to write the audio (inside the workspace); default: the profile's audio cache."},
                "speed": {"type": "number", "minimum": SPEED.0, "maximum": SPEED.1, "description": "Speaking speed (overrides the configured one)."},
                "instructions": {"type": "string", "description": "Style direction (tone, accent, pace) for providers that take it."}
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::Workspace;
    d.idempotent = false;
    d.claims = Some(ResourceClaims::none());
    d.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
    d
}

/// `elevenlabs` adds a `voice` to pick and an `action` to list voices.
fn parameters(schema: &Value, config: &Config) -> Value {
    if config.tts.provider.as_deref() != Some("elevenlabs") {
        return schema.clone();
    }
    let mut schema = schema.clone();
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.insert(
            "voice".into(),
            json!({"type": "string", "description": "An ElevenLabs voice id (see action list_voices)."}),
        );
        properties.insert(
            "action".into(),
            json!({"type": "string", "enum": ["speak", "list_voices"], "description": "list_voices returns the account's voices instead of speaking."}),
        );
    }
    schema["required"] = json!([]);
    schema
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.0.of(context)?;
            let result = session.text_to_speech(&arguments).await;
            usage::record(
                session.setup().usage.as_ref(),
                context,
                "text_to_speech",
                &result,
            );
            Ok(result)
        })
    }
}

/// `path` lexically resolved against `base` (no `..` escapes).
fn lexical(base: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in base.join(path).components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

impl MediaSession {
    /// Runs one `text_to_speech` call.
    pub async fn text_to_speech(&self, arguments: &Value) -> ToolResult {
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if text("action").as_deref() == Some("list_voices") {
            return self.list_voices().await;
        }
        let speed = arguments.get("speed").and_then(Value::as_f64);
        if speed.is_some_and(|s| !(SPEED.0..=SPEED.1).contains(&s)) {
            return ToolResult::error("text_to_speech: speed must be within 0.25..4.0");
        }
        let output = match text("output_path")
            .map(|p| self.output_path(&p))
            .transpose()
        {
            Ok(output) => output,
            Err(why) => return ToolResult::error(format!("text_to_speech: {why}")),
        };
        let options = Speak {
            speed,
            instructions: text("instructions"),
            voice: text("voice"),
        };
        let spoken = match self
            .synthesize(&text("text").unwrap_or_default(), &options, output)
            .await
        {
            Ok(spoken) => spoken,
            Err(why) => return ToolResult::error(format!("text_to_speech failed: {why}")),
        };
        let bubble = VOICE_BUBBLES.contains(&self.setup().surface.as_str());
        let modifier = if bubble { " [[as_voice]]" } else { "" };
        let details = json!({"mime": spoken.mime, "voice_compatible": spoken.voice_compatible,
            "provider": spoken.provider, "model": spoken.model, "requests": spoken.chunks});
        let mut result = ToolResult::ok(format!(
            "MEDIA:{}{modifier}\n{details}",
            spoken.path.display()
        ));
        result.metadata.insert(
            "voice_compatible".into(),
            Value::Bool(spoken.voice_compatible),
        );
        usage::mark(
            &mut result,
            &spoken.provider,
            &spoken.model,
            &spoken.billing_source,
        );
        result
    }

    /// `output_path` inside the workspace or the profile's audio cache.
    fn output_path(&self, requested: &str) -> Result<PathBuf, String> {
        let setup = self.setup();
        let path = lexical(&setup.workspace, requested);
        let cache = audio_dir(&setup.profile_dir);
        if path.starts_with(&setup.workspace) || path.starts_with(&cache) {
            Ok(path)
        } else {
            Err(format!(
                "output_path {requested} is outside the workspace and {}",
                cache.display()
            ))
        }
    }

    async fn list_voices(&self) -> ToolResult {
        let setup = self.setup();
        let voice = match super::tts::resolve(setup) {
            Ok(voice) if voice.provider == "elevenlabs" => voice,
            Ok(voice) => {
                return ToolResult::error(format!(
                    "text_to_speech: {} has no voice list",
                    voice.provider
                ));
            }
            Err(why) => return ToolResult::error(format!("text_to_speech unavailable: {why}")),
        };
        match super::tts::elevenlabs_voices(setup, &voice).await {
            Ok(voices) => {
                let list: Vec<Value> = voices
                    .into_iter()
                    .map(|(id, name)| json!({"voice": id, "name": name}))
                    .collect();
                ToolResult::ok(json!({"voices": list}).to_string())
            }
            Err(why) => ToolResult::error(format!("text_to_speech: voice list failed: {why}")),
        }
    }
}
