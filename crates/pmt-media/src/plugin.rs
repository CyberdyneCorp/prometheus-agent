//! Image generation, text-to-speech and transcription providers of
//! enabled plugins (spec: plugin-system, Provider-object plugins fail
//! closed when unregistered).
//!
//! `image_gen.provider`, `tts.provider` and `stt.provider` resolve a name
//! the same way: a built-in provider first (and, for speech, a declared
//! `[tts|stt.providers.<name>]` command), then the provider an enabled
//! plugin registered ([`MediaPlugins`], the session's plugin registry);
//! a name nothing provides keeps failing with `<category> provider
//! '<name>' is not installed or not enabled`.
//!
//! The JSON documents are the Hermes provider methods' keyword arguments
//! (the python bridge forwards them as is):
//! - `image_gen` (`generate`): `{"prompt", "aspect_ratio", "upscale",
//!   "model"?, "image_url"?, "reference_image_urls"?}` answered by
//!   `{"success", "image", "model"?, "upscaled"?, "error"?}`; `image` is
//!   a `data:` URL, an http(s) URL or a file in the plugin's data
//!   directory.
//! - `tts` (`synthesize`): `{"text", "output_path", "state_path",
//!   "format", "voice"?, "model"?, "speed"?, "instructions"?}` answered by
//!   the written file's path (a string or `{"path"}`) or `{"audio":
//!   <base64>}`.
//! - `stt` (`transcribe`): `{"file_path", "state_path", "language"?}`
//!   answered by `{"success", "transcript", "error"?}` or the text.
//!
//! Files are exchanged in `<profile>/plugin-data/<plugin>/files/media/`,
//! the one directory both a sandboxed python sidecar (its data
//! directory) and a WASM plugin (`state` files, `state_path` relative to
//! `files/`) reach. The host reads a reported file only inside the
//! plugin's own data directory and keeps it only when it really is an
//! image or audio, so an answer cannot smuggle another host file out.
//! Any answer may carry `billing_source` in the ledger grammar; without
//! one the call bills as `local`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pmt_config::image_models::image_provider;
use serde_json::{Value, json};

use crate::MediaSetup;
use crate::access::Access;
use crate::http::{data_url, read_capped, unb64};
use crate::image::providers::{Generated, MAX_IMAGE_BYTES, Request};
use crate::image::{Model, Plan};
use crate::speech::audio::Format;
use crate::speech::stt::Transcript;
use crate::speech::tts::{Speak, Voice};

/// A provider category a media tool selects by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `image_gen.provider`.
    ImageGen,
    /// `tts.provider`.
    Tts,
    /// `stt.provider`.
    Stt,
}

impl Kind {
    /// The category's name in errors (`image_gen provider '...'`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageGen => "image_gen",
            Self::Tts => "tts",
            Self::Stt => "stt",
        }
    }
}

/// The answer of one plugin provider call.
pub type PluginFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// A resolved transcription as `pre_transcription` callbacks see it.
pub type SttFuture<'a> = Pin<Box<dyn Future<Output = SttRequest> + Send + 'a>>;

/// A transcription after provider resolution (spec: plugin-system,
/// `pre_transcription`): what callbacks see, and the only fields they may
/// change are `prompt`, `language` and `model`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SttRequest {
    /// The resolved provider (never changed by a callback).
    pub provider: String,
    /// Its model.
    pub model: String,
    /// The spoken language (`None`: detected).
    pub language: Option<String>,
    /// A hint of vocabulary and style for the recogniser.
    pub prompt: Option<String>,
}

impl SttRequest {
    /// Applies one callback's answer: its string `prompt`, `language`
    /// and `model` replace the current values (an empty `prompt` or
    /// `language` clears it); every other field is ignored.
    pub fn apply(&mut self, answer: &Value) {
        let text = |key: &str| answer.get(key).and_then(Value::as_str).map(str::to_owned);
        if let Some(model) = text("model").filter(|m| !m.is_empty()) {
            self.model = model;
        }
        if let Some(language) = text("language") {
            self.language = Some(language).filter(|l| !l.is_empty());
        }
        if let Some(prompt) = text("prompt") {
            self.prompt = Some(prompt).filter(|p| !p.is_empty());
        }
    }
}

/// The providers enabled plugins registered.
pub trait MediaPlugins: Send + Sync {
    /// The plugin that registered provider `id` of `kind`, if one did.
    fn plugin_of(&self, kind: Kind, id: &str) -> Option<String>;

    /// Sends `request` to provider `id` of `kind` and returns its JSON
    /// answer.
    fn invoke<'a>(&'a self, kind: Kind, id: &'a str, request: Value) -> PluginFuture<'a>;

    /// Fires `pre_transcription` with `request` and returns it as the
    /// callbacks left it, applied in registration order (last writer
    /// wins). Without hooks it is returned unchanged.
    fn pre_transcription(&self, request: SttRequest) -> SttFuture<'_> {
        Box::pin(async move { request })
    }
}

/// No plugin providers (inspection, tests).
pub struct NoPlugins;

impl MediaPlugins for NoPlugins {
    fn plugin_of(&self, _: Kind, _: &str) -> Option<String> {
        None
    }

    fn invoke<'a>(&'a self, kind: Kind, id: &'a str, _: Value) -> PluginFuture<'a> {
        Box::pin(async move { Err(not_installed(kind, id)) })
    }
}

/// `<category> provider '<id>' is not installed or not enabled`.
#[must_use]
pub fn not_installed(kind: Kind, id: &str) -> String {
    format!(
        "{} provider '{id}' is not installed or not enabled",
        kind.as_str()
    )
}

/// The selected plugin provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    /// The configured name.
    pub id: String,
    /// The plugin that registered it.
    pub plugin: String,
}

/// The plugin provider `kind`'s config key selects: set, not a built-in
/// or declared command provider, and registered by an enabled plugin.
#[must_use]
pub fn selected(setup: &MediaSetup, kind: Kind) -> Option<Selected> {
    let config = setup.config();
    let (id, ours) = match kind {
        Kind::ImageGen => {
            let id = config.image_gen.provider.as_deref()?;
            (id, image_provider(id).is_some())
        }
        Kind::Tts => {
            let id = config.tts.provider.as_deref()?;
            let ours =
                crate::speech::tts::BUILTIN.contains(&id) || config.tts.providers.contains_key(id);
            (id, ours)
        }
        Kind::Stt => {
            let id = config.stt.provider.as_deref()?;
            let ours =
                crate::speech::stt::BUILTIN.contains(&id) || config.stt.providers.contains_key(id);
            (id, ours)
        }
    };
    if ours {
        return None;
    }
    let plugin = setup.plugins.plugin_of(kind, id)?;
    Some(Selected {
        id: id.to_owned(),
        plugin,
    })
}

/// The answer's `billing_source` when it follows the ledger grammar
/// (`subscription:`, `api_key:`, `keyless:`), else `local`.
#[must_use]
pub fn billing(answer: &Value) -> String {
    let reported = answer
        .get("billing_source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let valid = ["subscription:", "api_key:", "keyless:"].iter().any(|p| {
        reported
            .strip_prefix(p)
            .is_some_and(|rest| !rest.is_empty())
    });
    if valid {
        reported.to_owned()
    } else {
        "local".to_owned()
    }
}

/// The error of an answer with `"success": false`.
fn failure(answer: &Value) -> Option<String> {
    (answer.get("success") == Some(&Value::Bool(false))).then(|| {
        answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the provider failed")
            .to_owned()
    })
}

fn data_dir(setup: &MediaSetup, plugin: &str) -> PathBuf {
    setup.profile_dir.join("plugin-data").join(plugin)
}

/// A file both sides reach: its host path and its `state` path.
struct Exchange {
    path: PathBuf,
    state_path: String,
}

fn exchange(setup: &MediaSetup, plugin: &str, extension: &str) -> Result<Exchange, String> {
    let dir = data_dir(setup, plugin).join("files").join("media");
    pmt_core::private_fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let name = format!("{}.{extension}", ulid::Ulid::generate());
    Ok(Exchange {
        path: dir.join(&name),
        state_path: format!("media/{name}"),
    })
}

/// A file the plugin reported, read only inside its data directory;
/// returns the bytes and the resolved path.
async fn read_reported(
    setup: &MediaSetup,
    plugin: &str,
    path: &str,
) -> Result<(Vec<u8>, PathBuf), String> {
    let root = pmt_core::capability::canonical(&data_dir(setup, plugin));
    let real = pmt_core::capability::canonical(Path::new(path));
    if !Path::new(path).is_absolute() || !real.starts_with(&root) {
        return Err(format!(
            "the provider reported a file outside its plugin's data directory: {path}"
        ));
    }
    let bytes = tokio::fs::read(&real)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    Ok((bytes, real))
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// One `image_generate` call to the plugin provider: the plan the result
/// reports and the image.
///
/// # Errors
///
/// The plugin's failure, or an answer that holds no image.
pub async fn generate_image(
    setup: &MediaSetup,
    selected: &Selected,
    request: &Request,
) -> Result<(Plan, Generated), String> {
    let mut body = json!({"prompt": request.prompt, "aspect_ratio": request.aspect.name(),
        "upscale": request.upscale});
    if let Some(model) = &setup.config().image_gen.model {
        body["model"] = json!(model);
    }
    if let Some(url) = &request.image_url {
        body["image_url"] = json!(url);
    }
    if !request.references.is_empty() {
        body["reference_image_urls"] = json!(request.references);
    }
    let answer = setup
        .plugins
        .invoke(Kind::ImageGen, &selected.id, body)
        .await?;
    if let Some(why) = failure(&answer) {
        return Err(why);
    }
    let image = text(&answer, "image").ok_or("the provider answered no image")?;
    let (bytes, mime) = if let Some(decoded) = data_url(image) {
        decoded
    } else if image.starts_with("https://") || image.starts_with("http://") {
        let response = setup
            .fetcher
            .get(image)
            .await
            .map_err(|e| e.message().to_owned())?;
        read_capped(response, MAX_IMAGE_BYTES).await?
    } else {
        (
            read_reported(setup, &selected.plugin, image).await?.0,
            String::new(),
        )
    };
    if ::image::guess_format(&bytes).is_err() {
        return Err("the provider's answer is not an image".to_owned());
    }
    let model = text(&answer, "model")
        .map(str::to_owned)
        .or_else(|| setup.config().image_gen.model.clone())
        .unwrap_or_default();
    let plan = Plan {
        provider: selected.id.clone(),
        // The plugin decides what upscaling means; the result reports
        // what it said it did.
        model: Model {
            upscaler: true,
            ..Model::verbatim(&model)
        },
        access: Access {
            token: None,
            billing_source: billing(&answer),
            gateway_url: None,
        },
        base_url: String::new(),
    };
    let generated = Generated {
        bytes,
        mime,
        upscaled: answer.get("upscaled") == Some(&Value::Bool(true)),
        ..Generated::default()
    };
    Ok((plan, generated))
}

/// The audio of one chunk from the plugin provider of `voice`, and how it
/// billed.
///
/// # Errors
///
/// The plugin's failure, or an answer that holds no audio.
pub async fn speak(
    setup: &MediaSetup,
    voice: &Voice,
    plugin: &str,
    chunk: &str,
    options: &Speak,
) -> Result<(Vec<u8>, String), String> {
    let out = exchange(setup, plugin, "mp3")?;
    let mut body = json!({"text": chunk, "output_path": out.path, "state_path": out.state_path,
        "format": "mp3"});
    for (key, value) in [
        ("voice", options.voice.as_deref()),
        ("instructions", options.instructions.as_deref()),
    ] {
        if let Some(value) = value {
            body[key] = json!(value);
        }
    }
    if let Some(speed) = options.speed {
        body["speed"] = json!(speed);
    }
    let answer = setup.plugins.invoke(Kind::Tts, &voice.provider, body).await;
    let spoken = audio_of(setup, plugin, &out.path, answer).await;
    let _ = tokio::fs::remove_file(&out.path).await;
    spoken
}

async fn audio_of(
    setup: &MediaSetup,
    plugin: &str,
    offered: &Path,
    answer: Result<Value, String>,
) -> Result<(Vec<u8>, String), String> {
    let answer = answer?;
    if let Some(why) = failure(&answer) {
        return Err(why);
    }
    let bytes = if let Some(audio) = text(&answer, "audio") {
        unb64(audio)?
    } else {
        let offered = offered.to_string_lossy();
        let path = answer
            .as_str()
            .or_else(|| text(&answer, "path"))
            .unwrap_or(&offered);
        // The reported file is consumed (it may differ from the offered
        // one: another format, another extension).
        let (bytes, real) = read_reported(setup, plugin, path).await?;
        let _ = tokio::fs::remove_file(&real).await;
        bytes
    };
    if Format::sniff(&bytes) == Format::Other {
        return Err("the provider's answer is not audio".to_owned());
    }
    Ok((bytes, billing(&answer)))
}

/// The transcript of the audio at `path` from the plugin provider.
///
/// # Errors
///
/// The plugin's failure.
pub async fn transcribe(
    setup: &MediaSetup,
    selected: &Selected,
    path: &Path,
    request: &SttRequest,
) -> Result<Transcript, String> {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("ogg");
    let copy = exchange(setup, &selected.plugin, extension)?;
    tokio::fs::copy(path, &copy.path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut body = json!({"file_path": copy.path, "state_path": copy.state_path});
    if let Some(language) = &request.language {
        body["language"] = json!(language);
    }
    if !request.model.is_empty() {
        body["model"] = json!(request.model);
    }
    if let Some(prompt) = &request.prompt {
        body["prompt"] = json!(prompt);
    }
    let answer = setup.plugins.invoke(Kind::Stt, &selected.id, body).await;
    let _ = tokio::fs::remove_file(&copy.path).await;
    let answer = answer?;
    if let Some(why) = failure(&answer) {
        return Err(why);
    }
    let said = answer
        .as_str()
        .or_else(|| text(&answer, "transcript"))
        .or_else(|| text(&answer, "text"))
        .unwrap_or_default();
    Ok(Transcript {
        text: said.trim().to_owned(),
        provider: selected.id.clone(),
        model: text(&answer, "model").unwrap_or(&request.model).to_owned(),
        billing_source: billing(&answer),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_billing_counts_only_in_the_ledger_grammar() {
        assert_eq!(
            billing(&json!({"billing_source": "api_key:acme"})),
            "api_key:acme"
        );
        assert_eq!(
            billing(&json!({"billing_source": "subscription:nous/team"})),
            "subscription:nous/team"
        );
        assert_eq!(billing(&json!({"billing_source": "free"})), "local");
        assert_eq!(billing(&json!({"billing_source": "api_key:"})), "local");
        assert_eq!(billing(&json!("a path")), "local");
    }
}
