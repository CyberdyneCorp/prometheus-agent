//! The `yuanbao` toolset of the Yuanbao catalog adapter (spec:
//! platform-adapters, Feishu and Yuanbao tools): group info and members,
//! a DM to a group member found by name, and the sticker pack. They act
//! over the adapter's own WebSocket, so each call goes to the Yuanbao
//! adapter connected for the profile in this process
//! ([`crate::plugins_host::adapter_tools`]); its arguments are checked
//! against the schemas below first (`yb_search_sticker` takes at most 50).

use std::path::PathBuf;
use std::sync::Arc;

use pmt_tools::descriptor::{CallContext, Effect, HandlerFuture, Owner, ToolHandler};
use pmt_tools::platform_tools::{Context, Contributor};
use pmt_tools::{RegisteredTool, ToolDescriptor, ToolFailure, ToolResult};
use serde_json::{Value, json};

/// The platform (the catalog plugin's id).
pub const PLATFORM: &str = "yuanbao";
/// The most `yb_search_sticker` returns.
pub const MAX_STICKERS: u64 = 50;

struct Handler {
    name: &'static str,
    profile_dir: PathBuf,
}

impl Handler {
    async fn run(&self, args: Value) -> Result<ToolResult, ToolFailure> {
        // The schema bounds `limit`; a caller that skipped validation is
        // refused here as well, never clamped silently.
        if args["limit"].as_u64().is_some_and(|l| l > MAX_STICKERS) {
            return Err(ToolFailure::new(
                "invalid_input",
                format!("limit must be at most {MAX_STICKERS}"),
            ));
        }
        crate::plugins_host::adapter_tools::call(&self.profile_dir, PLATFORM, self.name, &args)
            .await
            .map(ToolResult::ok)
            .map_err(|why| ToolFailure::new("yuanbao", why))
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

/// Each tool: name, description, schema, external effect.
fn catalog() -> [(&'static str, &'static str, Value, bool); 5] {
    let group = json!({"type": "string", "description": "The group code."});
    let chat = json!({"type": "string", "description": "direct:<account> or group:<code>; the current conversation when absent."});
    [
        (
            "yb_query_group_info",
            "Read a Yuanbao group's name, owner and member count.",
            schema(&json!({"group_code": group}), &["group_code"]),
            false,
        ),
        (
            "yb_query_group_members",
            "List a Yuanbao group's members: find by name, only the bots, or all.",
            schema(
                &json!({
                    "group_code": group,
                    "action": {"type": "string", "enum": ["find", "list_bots", "list_all"]},
                    "name": {"type": "string", "description": "Part of a nickname (action find)."},
                }),
                &["group_code", "action"],
            ),
            false,
        ),
        (
            "yb_send_dm",
            "Send a direct message to a member of a Yuanbao group, found by name.",
            schema(
                &json!({
                    "name": {"type": "string", "description": "The member's nickname (or part of it)."},
                    "text": {"type": "string"},
                    "media": {"type": "string", "description": "A sticker name to send with it."},
                    "group_code": group,
                }),
                &["name", "text"],
            ),
            true,
        ),
        (
            "yb_search_sticker",
            "Search the built-in Yuanbao sticker pack by name or meaning.",
            schema(
                &json!({
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_STICKERS, "default": 10},
                }),
                &["query"],
            ),
            false,
        ),
        (
            "yb_send_sticker",
            "Send a sticker (by name or id) in a Yuanbao conversation.",
            schema(
                &json!({
                    "sticker": {"type": "string"},
                    "ref_msg_id": {"type": "string", "description": "The message it answers."},
                    "chat": chat,
                }),
                &["sticker"],
            ),
            true,
        ),
    ]
}

/// The toolset for every profile that enables the platform.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tools;

impl Contributor for Tools {
    fn platform(&self) -> &str {
        PLATFORM
    }

    fn toolsets(&self) -> Vec<(String, String)> {
        vec![(
            PLATFORM.to_owned(),
            "Yuanbao groups, member DMs and stickers".to_owned(),
        )]
    }

    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool> {
        catalog()
            .into_iter()
            .map(|(name, description, parameters, external)| {
                let mut descriptor = ToolDescriptor::new(name, PLATFORM, description, parameters);
                if !external {
                    descriptor.effect = Effect::None;
                    descriptor.idempotent = true;
                }
                let handler = Handler {
                    name,
                    profile_dir: context.profile_dir.to_path_buf(),
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
