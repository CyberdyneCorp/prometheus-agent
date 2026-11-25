//! The `feishu_doc` and `feishu_drive` toolsets of the Feishu catalog
//! adapter (spec: platform-adapters, Feishu and Yuanbao tools): read a
//! document as plain text, list a document's comments and a comment's
//! replies, reply to a comment and add a whole-document comment. They
//! call the Feishu (or Lark) Open API directly with the app's tenant
//! access token, from `[gateway.platforms.feishu]` option `app_id` and
//! the secret `FEISHU_APP_SECRET`; `options.domain` (`feishu` | `lark`)
//! or `options.api_base` picks the endpoint.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_tools::descriptor::{CallContext, Effect, HandlerFuture, Owner, ToolHandler};
use pmt_tools::platform_tools::{Context, Contributor, SecretReader};
use pmt_tools::{RegisteredTool, ToolDescriptor, ToolFailure, ToolResult};
use serde_json::{Value, json};

/// The platform (the catalog plugin's id).
pub const PLATFORM: &str = "feishu";
/// The app secret.
pub const SECRET: &str = "FEISHU_APP_SECRET";
/// The largest page a listing asks for.
pub const MAX_PAGE: u64 = 100;
const DOC: &str = "feishu_doc";
const DRIVE: &str = "feishu_drive";

/// The Open API origin of the platform options.
#[must_use]
pub fn origin(options: &std::collections::BTreeMap<String, Value>) -> String {
    if let Some(base) = options.get("api_base").and_then(Value::as_str) {
        return base.trim_end_matches('/').to_owned();
    }
    match options.get("domain").and_then(Value::as_str) {
        Some("lark") => "https://open.larksuite.com".to_owned(),
        _ => "https://open.feishu.cn".to_owned(),
    }
}

fn invalid(message: impl Into<String>) -> ToolFailure {
    ToolFailure::new("invalid_input", message)
}

/// A token or id argument: letters, digits, `_` and `-` only, so it
/// stays one path segment.
fn token<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolFailure> {
    let value = args[key]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(format!("{key} is required")))?;
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(value)
    } else {
        Err(invalid(format!("{key} '{value}' is not a Feishu token")))
    }
}

fn file_type(args: &Value) -> Result<String, ToolFailure> {
    match args["file_type"].as_str() {
        None | Some("") => Ok("docx".to_owned()),
        Some(_) => token(args, "file_type").map(str::to_owned),
    }
}

fn page_size(args: &Value) -> Result<u64, ToolFailure> {
    match args.get("page_size").filter(|v| !v.is_null()) {
        None => Ok(MAX_PAGE),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_PAGE).contains(&n) => Ok(n),
            _ => Err(invalid(format!("page_size must be 1 to {MAX_PAGE}"))),
        },
    }
}

fn content(args: &Value) -> Result<String, ToolFailure> {
    let text = args["content"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| invalid("content is required"))?;
    // Feishu's comment text cannot hold raw `<`, `>` or `&`.
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;"))
}

/// The app's credentials and its cached tenant token.
struct Client {
    http: reqwest::Client,
    base: String,
    app_id: String,
    secrets: SecretReader,
    token: Mutex<Option<(String, Instant)>>,
}

fn failure(message: impl Into<String>) -> ToolFailure {
    ToolFailure::new("feishu", message)
}

/// The answer's `data`, or why Feishu refused.
fn data_of(status: reqwest::StatusCode, answer: &Value) -> Result<Value, ToolFailure> {
    let code = answer["code"].as_i64().unwrap_or(-1);
    if status.is_success() && code == 0 {
        return Ok(answer["data"].clone());
    }
    Err(failure(format!(
        "Feishu refused ({code}, HTTP {status}): {}",
        answer["msg"].as_str().unwrap_or("no message")
    )))
}

impl Client {
    async fn token(&self) -> Result<String, ToolFailure> {
        if let Some((token, until)) = self.token.lock().map_err(|_| failure("poisoned"))?.clone()
            && Instant::now() < until
        {
            return Ok(token);
        }
        if self.app_id.is_empty() {
            return Err(failure(
                "gateway.platforms.feishu.options.app_id is not set (run `prometheus gateway setup feishu`)",
            ));
        }
        let secret = (self.secrets)(SECRET).ok_or_else(|| {
            ToolFailure::new("missing_secret", format!("secret {SECRET} is not set"))
        })?;
        let response = self
            .http
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.base
            ))
            .json(&json!({"app_id": self.app_id, "app_secret": secret}))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| ToolFailure::new("network", format!("Feishu is unreachable: {e}")))?;
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() || answer["code"].as_i64() != Some(0) {
            return Err(failure(format!(
                "Feishu refused the app credentials: {}",
                answer["msg"].as_str().unwrap_or("no message")
            )));
        }
        let token = answer["tenant_access_token"]
            .as_str()
            .ok_or_else(|| failure("Feishu sent no tenant_access_token"))?
            .to_owned();
        let life = answer["expire"]
            .as_u64()
            .unwrap_or(7200)
            .saturating_sub(300);
        *self.token.lock().map_err(|_| failure("poisoned"))? =
            Some((token.clone(), Instant::now() + Duration::from_secs(life)));
        Ok(token)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, ToolFailure> {
        let token = self.token().await?;
        let mut url = url::Url::parse(&format!("{}{path}", self.base))
            .map_err(|e| failure(format!("bad Feishu endpoint: {e}")))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(30));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ToolFailure::new("network", format!("Feishu is unreachable: {e}")))?;
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        data_of(status, &answer)
    }
}

/// Which tool a handler is.
#[derive(Debug, Clone, Copy)]
enum Kind {
    DocRead,
    ListComments,
    ListReplies,
    Reply,
    AddComment,
}

struct Handler {
    kind: Kind,
    client: Arc<Client>,
}

fn listing(
    args: &Value,
    extra: &[(&'static str, String)],
) -> Result<Vec<(&'static str, String)>, ToolFailure> {
    let mut query = vec![
        ("file_type", file_type(args)?),
        ("user_id_type", "open_id".to_owned()),
        ("page_size", page_size(args)?.to_string()),
    ];
    query.extend(extra.iter().cloned());
    if let Some(page) = args["page_token"].as_str().filter(|p| !p.is_empty()) {
        query.push(("page_token", page.to_owned()));
    }
    Ok(query)
}

impl Handler {
    async fn run(&self, args: Value) -> Result<ToolResult, ToolFailure> {
        use reqwest::Method;
        let answer = match self.kind {
            Kind::DocRead => {
                let path = format!(
                    "/open-apis/docx/v1/documents/{}/raw_content",
                    token(&args, "doc_token")?
                );
                let data = self.client.call(Method::GET, &path, &[], None).await?;
                return Ok(ToolResult::ok(data["content"].as_str().unwrap_or_default()));
            }
            Kind::ListComments => {
                let path = format!(
                    "/open-apis/drive/v1/files/{}/comments",
                    token(&args, "file_token")?
                );
                let whole = args["is_whole"].as_bool().unwrap_or(false).to_string();
                let query = listing(&args, &[("is_whole", whole)])?;
                self.client.call(Method::GET, &path, &query, None).await?
            }
            Kind::ListReplies => {
                let path = format!(
                    "/open-apis/drive/v1/files/{}/comments/{}/replies",
                    token(&args, "file_token")?,
                    token(&args, "comment_id")?
                );
                let query = listing(&args, &[])?;
                self.client.call(Method::GET, &path, &query, None).await?
            }
            Kind::Reply => {
                let path = format!(
                    "/open-apis/drive/v1/files/{}/comments/{}/replies",
                    token(&args, "file_token")?,
                    token(&args, "comment_id")?
                );
                let body = json!({"content": {"elements": [{"type": "text_run", "text_run": {"text": content(&args)?}}]}});
                let query = [("file_type", file_type(&args)?)];
                self.client
                    .call(Method::POST, &path, &query, Some(body))
                    .await?
            }
            Kind::AddComment => {
                let path = format!(
                    "/open-apis/drive/v1/files/{}/new_comments",
                    token(&args, "file_token")?
                );
                let body = json!({"file_type": file_type(&args)?, "reply_elements": [{"type": "text", "text": content(&args)?}]});
                self.client
                    .call(Method::POST, &path, &[], Some(body))
                    .await?
            }
        };
        Ok(ToolResult::ok(answer.to_string()))
    }
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(self.run(arguments))
    }
}

fn schema(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// Each tool: name, toolset, description, schema, kind, external effect.
fn catalog() -> [(&'static str, &'static str, &'static str, Value, Kind, bool); 5] {
    let file = json!({"type": "string", "description": "The document's file token."});
    let kind = json!({"type": "string", "description": "The file type (docx, doc, sheet, ...).", "default": "docx"});
    let page = json!({"type": "integer", "minimum": 1, "maximum": MAX_PAGE, "default": MAX_PAGE});
    let cursor =
        json!({"type": "string", "description": "The next page's token from the previous answer."});
    let comment = json!({"type": "string", "description": "The comment id."});
    let text = json!({"type": "string", "description": "Plain text."});
    [
        (
            "feishu_doc_read",
            DOC,
            "Read a Feishu or Lark document (docx) as plain text.",
            schema(
                &json!({"doc_token": {"type": "string", "description": "The document token (from its URL or a comment's context)."}}),
                &["doc_token"],
            ),
            Kind::DocRead,
            false,
        ),
        (
            "feishu_drive_list_comments",
            DRIVE,
            "List a Feishu document's comments (whole-document ones with is_whole).",
            schema(
                &json!({"file_token": file, "file_type": kind, "is_whole": {"type": "boolean", "default": false}, "page_size": page, "page_token": cursor}),
                &["file_token"],
            ),
            Kind::ListComments,
            false,
        ),
        (
            "feishu_drive_list_comment_replies",
            DRIVE,
            "List the replies in one comment thread of a Feishu document.",
            schema(
                &json!({"file_token": file, "comment_id": comment, "file_type": kind, "page_size": page, "page_token": cursor}),
                &["file_token", "comment_id"],
            ),
            Kind::ListReplies,
            false,
        ),
        (
            "feishu_drive_reply_comment",
            DRIVE,
            "Reply in a comment thread of a Feishu document.",
            schema(
                &json!({"file_token": file, "comment_id": comment, "content": text, "file_type": kind}),
                &["file_token", "comment_id", "content"],
            ),
            Kind::Reply,
            true,
        ),
        (
            "feishu_drive_add_comment",
            DRIVE,
            "Add a whole-document comment to a Feishu document.",
            schema(
                &json!({"file_token": file, "content": text, "file_type": kind}),
                &["file_token", "content"],
            ),
            Kind::AddComment,
            true,
        ),
    ]
}

/// The toolsets for every profile that enables the platform.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tools;

impl Contributor for Tools {
    fn platform(&self) -> &str {
        PLATFORM
    }

    fn toolsets(&self) -> Vec<(String, String)> {
        vec![
            (DOC.to_owned(), "Read Feishu documents".to_owned()),
            (
                DRIVE.to_owned(),
                "Read and answer Feishu document comments".to_owned(),
            ),
        ]
    }

    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool> {
        let http = match pmt_net::http::client(context.network) {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("Feishu tools unavailable: {}", e.message());
                return Vec::new();
            }
        };
        let options = &context.config.options;
        let client = Arc::new(Client {
            http,
            base: origin(options),
            app_id: options
                .get("app_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            secrets: Arc::clone(&context.secrets),
            token: Mutex::new(None),
        });
        catalog()
            .into_iter()
            .map(|(name, toolset, description, parameters, kind, external)| {
                let mut descriptor = ToolDescriptor::new(name, toolset, description, parameters);
                if !external {
                    descriptor.effect = Effect::None;
                    descriptor.idempotent = true;
                }
                let handler = Handler {
                    kind,
                    client: Arc::clone(&client),
                };
                RegisteredTool::new(
                    descriptor,
                    Owner::Capability(format!("platform:{PLATFORM}")),
                    Arc::new(handler),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_stay_one_path_segment_and_pages_are_bounded() {
        assert!(token(&json!({"t": "doxcn_A-1"}), "t").is_ok());
        let err = token(&json!({"t": "../x"}), "t").unwrap_err();
        assert!(err.message.contains("not a Feishu token"), "{err:?}");
        assert_eq!(page_size(&json!({})).unwrap(), 100);
        assert!(page_size(&json!({"page_size": 101})).is_err());
        assert_eq!(file_type(&json!({})).unwrap(), "docx");
        assert_eq!(content(&json!({"content": "a<b"})).unwrap(), "a&lt;b");
        let options = serde_json::from_value(json!({"domain": "lark"})).unwrap();
        assert_eq!(origin(&options), "https://open.larksuite.com");
    }
}
