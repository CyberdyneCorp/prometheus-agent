//! How each image provider is asked (spec: web-and-media-tools, Built-in
//! image generation providers and models): fal.ai's model endpoints, the
//! OpenAI-compatible Images API (`openai`, `xai`, `krea`, `meta-ai`,
//! `deepinfra`), OpenRouter's image-output chat completions, the Codex
//! sign-in's Responses image tool and the subscription tool gateway.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};

use super::Plan;
use crate::http::{data_url, download, ok_json, unb64};

/// Largest generated image downloaded.
pub const MAX_IMAGE_BYTES: u64 = 50 << 20;

/// The requested framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aspect {
    /// Wide (default).
    Landscape,
    /// 1:1.
    Square,
    /// Tall.
    Portrait,
}

impl Aspect {
    /// The argument's spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "landscape" => Some(Self::Landscape),
            "square" => Some(Self::Square),
            "portrait" => Some(Self::Portrait),
            _ => None,
        }
    }

    fn fal(self) -> &'static str {
        match self {
            Self::Landscape => "landscape_16_9",
            Self::Square => "square_hd",
            Self::Portrait => "portrait_16_9",
        }
    }

    /// The pixel size asked of the Images API.
    #[must_use]
    pub fn size(self) -> &'static str {
        match self {
            Self::Landscape => "1536x1024",
            Self::Square => "1024x1024",
            Self::Portrait => "1024x1536",
        }
    }

    fn ratio(self) -> &'static str {
        match self {
            Self::Landscape => "16:9",
            Self::Square => "1:1",
            Self::Portrait => "9:16",
        }
    }
}

/// One generation request.
#[derive(Debug, Clone)]
pub struct Request {
    /// The prompt.
    pub prompt: String,
    /// The framing.
    pub aspect: Aspect,
    /// A source image to edit.
    pub image_url: Option<String>,
    /// Reference images.
    pub references: Vec<String>,
    /// Upscale (only when the model has an upscaler).
    pub upscale: bool,
}

impl Request {
    fn inputs(&self) -> Vec<&str> {
        self.image_url
            .iter()
            .chain(&self.references)
            .map(String::as_str)
            .collect()
    }
}

/// A generated image.
#[derive(Debug, Clone, Default)]
pub struct Generated {
    /// Its bytes.
    pub bytes: Vec<u8>,
    /// Its MIME type (as reported; sniffed later when empty).
    pub mime: String,
    /// The quality the provider reports producing.
    pub quality: Option<String>,
    /// The size the provider reports producing.
    pub size: Option<String>,
    /// The output was upscaled.
    pub upscaled: bool,
}

/// Generates per `plan`.
///
/// # Errors
///
/// The provider's failure.
pub async fn generate(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    match plan.provider.as_str() {
        "fal" => fal(client, plan, request).await,
        "openrouter" => openrouter(client, plan, request).await,
        "openai-codex" => codex(client, plan, request).await,
        "nous" => gateway(client, plan, request).await,
        _ => images_api(client, plan, request).await,
    }
}

fn bearer(plan: &Plan) -> Result<String, String> {
    Ok(format!("Bearer {}", plan.access.bearer()?.expose()))
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    auth: &str,
    body: &Value,
) -> Result<Value, String> {
    ok_json(
        client
            .post(url)
            .header(AUTHORIZATION, auth)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await,
    )
    .await
}

/// The image of a reply item: inline base64, a `data:` URL or a URL to
/// download.
async fn fetch_image(client: &reqwest::Client, item: &Value) -> Result<(Vec<u8>, String), String> {
    if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
        let mime = item
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Ok((unb64(b64)?, mime.to_owned()));
    }
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "the reply holds no image".to_owned())?;
    if let Some(decoded) = data_url(url) {
        return Ok(decoded);
    }
    download(client, url, MAX_IMAGE_BYTES).await
}

async fn fal(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    let auth = format!("Key {}", plan.access.bearer()?.expose());
    let inputs = request.inputs();
    let endpoint = if inputs.is_empty() {
        plan.model.api_model.clone()
    } else {
        format!("{}/edit", plan.model.api_model)
    };
    let mut body =
        json!({"prompt": request.prompt, "image_size": request.aspect.fal(), "num_images": 1});
    if let Some(quality) = &plan.model.quality {
        body["quality"] = json!(quality);
    }
    if !inputs.is_empty() {
        body["image_urls"] = json!(inputs);
    }
    let reply = post(
        client,
        &format!("{}/{endpoint}", plan.base_url),
        &auth,
        &body,
    )
    .await?;
    let image = reply
        .pointer("/images/0")
        .ok_or("the reply holds no image")?;
    let mut url = image
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let upscaled = request.upscale && plan.model.upscaler;
    if upscaled {
        let reply = post(
            client,
            &format!("{}/fal-ai/clarity-upscaler", plan.base_url),
            &auth,
            &json!({"image_url": url}),
        )
        .await?;
        reply
            .pointer("/image/url")
            .and_then(Value::as_str)
            .ok_or("the upscaler returned no image")?
            .clone_into(&mut url);
    }
    let (bytes, mime) = fetch_image(client, &json!({"url": url})).await?;
    Ok(Generated {
        bytes,
        mime,
        quality: plan.model.quality.clone(),
        size: None,
        upscaled,
    })
}

/// The OpenAI Images API and its compatible endpoints.
async fn images_api(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    let inputs = request.inputs();
    let mut body = json!({"model": plan.model.api_model, "prompt": request.prompt, "n": 1,
        "size": request.aspect.size()});
    if let Some(quality) = &plan.model.quality {
        body["quality"] = json!(quality);
    }
    if plan.provider == "xai" {
        body["response_format"] = json!("b64_json");
        body.as_object_mut().map(|b| b.remove("size"));
    }
    let path = if inputs.is_empty() {
        "images/generations"
    } else {
        body["images"] = inputs.iter().map(|u| json!({"image_url": u})).collect();
        "images/edits"
    };
    let reply = post(
        client,
        &format!("{}/{path}", plan.base_url),
        &bearer(plan)?,
        &body,
    )
    .await?;
    let item = reply.pointer("/data/0").ok_or("the reply holds no image")?;
    let (bytes, mime) = fetch_image(client, item).await?;
    Ok(Generated {
        bytes,
        mime,
        quality: reply
            .get("quality")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| plan.model.quality.clone()),
        size: reply.get("size").and_then(Value::as_str).map(str::to_owned),
        upscaled: false,
    })
}

async fn openrouter(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    let mut content = vec![json!({"type": "text", "text": request.prompt})];
    content.extend(
        request
            .inputs()
            .into_iter()
            .map(|u| json!({"type": "image_url", "image_url": {"url": u}})),
    );
    let body = json!({
        "model": plan.model.api_model,
        "messages": [{"role": "user", "content": content}],
        "modalities": ["image", "text"],
        "image_config": {"aspect_ratio": request.aspect.ratio()},
    });
    let reply = post(
        client,
        &format!("{}/chat/completions", plan.base_url),
        &bearer(plan)?,
        &body,
    )
    .await?;
    let url = reply
        .pointer("/choices/0/message/images/0/image_url/url")
        .and_then(Value::as_str)
        .ok_or("the model returned no image")?;
    let (bytes, mime) = fetch_image(client, &json!({"url": url})).await?;
    Ok(Generated {
        bytes,
        mime,
        ..Generated::default()
    })
}

/// The Codex sign-in's Responses `image_generation` tool: the reply says
/// which quality and size it actually produced.
async fn codex(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    let mut tool = json!({"type": "image_generation", "model": plan.model.api_model,
        "size": request.aspect.size()});
    if let Some(quality) = &plan.model.quality {
        tool["quality"] = json!(quality);
    }
    let mut content = vec![json!({"type": "input_text", "text": request.prompt})];
    content.extend(
        request
            .inputs()
            .into_iter()
            .map(|u| json!({"type": "input_image", "image_url": u})),
    );
    let body = json!({
        "model": "gpt-5",
        "input": [{"role": "user", "content": content}],
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "store": false,
    });
    let reply = post(
        client,
        &format!("{}/responses", plan.base_url),
        &bearer(plan)?,
        &body,
    )
    .await?;
    let call = reply
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "image_generation_call")
        .ok_or("the reply holds no image")?;
    let result = call["result"]
        .as_str()
        .ok_or("the image call has no result")?;
    Ok(Generated {
        bytes: unb64(result)?,
        mime: call["output_format"]
            .as_str()
            .map(|f| format!("image/{f}"))
            .unwrap_or_default(),
        quality: call["quality"].as_str().map(str::to_owned),
        size: call["size"].as_str().map(str::to_owned),
        upscaled: false,
    })
}

async fn gateway(
    client: &reqwest::Client,
    plan: &Plan,
    request: &Request,
) -> Result<Generated, String> {
    let mut body = json!({"model": plan.model.api_model, "prompt": request.prompt,
        "aspect_ratio": request.aspect.ratio(), "upscale": request.upscale && plan.model.upscaler});
    if let Some(quality) = &plan.model.quality {
        body["quality"] = json!(quality);
    }
    if let Some(url) = &request.image_url {
        body["image_url"] = json!(url);
    }
    if !request.references.is_empty() {
        body["reference_image_urls"] = json!(request.references);
    }
    let routing = pmt_providers::auth::gateway::ToolRouting::Gateway {
        url: plan.base_url.clone(),
        token: plan.access.bearer()?.clone(),
        billing_source: plan.access.billing_source.clone(),
    };
    let reply =
        pmt_providers::auth::gateway::post(client, &routing, "/v1/images/generations", &body)
            .await
            .map_err(|e| e.message().to_owned())?;
    let item = reply
        .pointer("/images/0")
        .ok_or("the reply holds no image")?;
    let (bytes, mime) = fetch_image(client, item).await?;
    Ok(Generated {
        bytes,
        mime,
        quality: plan.model.quality.clone(),
        size: None,
        upscaled: reply["upscaled"].as_bool().unwrap_or(false),
    })
}
