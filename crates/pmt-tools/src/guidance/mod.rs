//! The first-party `security-guidance` hook plugin (spec:
//! security-boundaries, Security guidance on written code).
//!
//! When `security-guidance` is in `plugins.enabled` it matches the content
//! that `write_file`, `patch` and skill writes (`skill_manage`) put on disk
//! against [`rules::RULES`]. With `plugins.security_guidance.mode =
//! "warn"` (the default) the write proceeds and its result carries the
//! matched rule ids and guidance; with `block` the write is refused with
//! the guidance as the reason; `off` matches nothing. It runs natively as
//! a tool hook in front of the WASM plugins' hooks, so it costs no tool
//! surface and no prompt text.

pub mod rules;

use std::sync::Arc;

use pmt_config::schema::GuidanceMode;
use serde_json::{Value, json};

use crate::dispatch::{HookCall, PreDecision, SeamFuture, ToolHooks};
use crate::result::{Content, Status, ToolResult};

/// The plugin id that enables it in `plugins.enabled`.
pub const PLUGIN_ID: &str = "security-guidance";

/// Tools whose arguments carry written content.
const WRITERS: &[&str] = &["write_file", "patch", "skill_manage"];

/// Argument keys holding the written text.
const CONTENT_KEYS: &[&str] = &["content", "new_string", "patch", "file_content"];

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The rule id.
    pub id: &'static str,
    /// Its class.
    pub category: &'static str,
    /// What to do instead.
    pub guidance: &'static str,
}

/// The findings in `text`, in rule order.
#[must_use]
pub fn scan(text: &str) -> Vec<Finding> {
    rules::RULES
        .iter()
        .filter(|(rule, regex)| rule.matches(regex, text))
        .map(|(rule, _)| Finding {
            id: rule.id,
            category: rule.category,
            guidance: rule.guidance,
        })
        .collect()
}

/// The findings in what `call` writes.
fn findings(call: &HookCall<'_>) -> Vec<Finding> {
    written(call).map(|text| scan(&text)).unwrap_or_default()
}

/// The text a call writes, when the tool writes files.
fn written(call: &HookCall<'_>) -> Option<String> {
    if !WRITERS.contains(&call.tool) {
        return None;
    }
    let parts: Vec<&str> = CONTENT_KEYS
        .iter()
        .filter_map(|key| call.arguments.get(key).and_then(Value::as_str))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn describe(findings: &[Finding]) -> String {
    let lines: Vec<String> = findings
        .iter()
        .map(|f| format!("- [{}] {}: {}", f.id, f.category, f.guidance))
        .collect();
    format!(
        "security-guidance (heuristic findings, review before relying on this code):\n{}",
        lines.join("\n")
    )
}

/// The hook: guidance first, then `inner` (the plugins' hooks).
pub struct GuidanceHooks {
    mode: GuidanceMode,
    inner: Arc<dyn ToolHooks>,
}

impl GuidanceHooks {
    /// Guidance in `mode` in front of `inner`; `inner` alone when `mode`
    /// is off.
    #[must_use]
    pub fn wrap(mode: GuidanceMode, inner: Arc<dyn ToolHooks>) -> Arc<dyn ToolHooks> {
        if mode == GuidanceMode::Off {
            return inner;
        }
        Arc::new(Self { mode, inner })
    }
}

impl ToolHooks for GuidanceHooks {
    fn pre_tool_call<'a>(&'a self, call: HookCall<'a>) -> SeamFuture<'a, PreDecision> {
        Box::pin(async move {
            if self.mode == GuidanceMode::Block {
                let findings = findings(&call);
                if !findings.is_empty() {
                    return PreDecision::Block(format!(
                        "BLOCKED by security-guidance: the write was refused.\n{}",
                        describe(&findings)
                    ));
                }
            }
            self.inner.pre_tool_call(call).await
        })
    }

    fn post_tool_call<'a>(
        &'a self,
        call: HookCall<'a>,
        status: Status,
        duration_ms: u64,
    ) -> SeamFuture<'a, ()> {
        self.inner.post_tool_call(call, status, duration_ms)
    }

    fn transform_tool_result<'a>(
        &'a self,
        call: HookCall<'a>,
        result: ToolResult,
        duration_ms: u64,
    ) -> SeamFuture<'a, ToolResult> {
        Box::pin(async move {
            let mut result = self
                .inner
                .transform_tool_result(call, result, duration_ms)
                .await;
            if self.mode != GuidanceMode::Warn || result.status != Status::Ok {
                return result;
            }
            let findings = findings(&call);
            if findings.is_empty() {
                return result;
            }
            let listed: Vec<Value> = findings
                .iter()
                .map(|f| json!({"rule": f.id, "category": f.category, "guidance": f.guidance}))
                .collect();
            result
                .metadata
                .insert("security_guidance".to_owned(), Value::Array(listed));
            result.content.push(Content::Text {
                text: format!("\n\n{}", describe(&findings)),
            });
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{rules::RULES, scan};

    #[test]
    fn the_rule_set_covers_every_class_with_at_least_25_rules() {
        assert!(RULES.len() >= 25, "{}", RULES.len());
        for category in [
            "unsafe deserialization",
            "command injection",
            "XSS sink",
            "weak cryptography",
            "XXE",
            "missing subresource integrity",
            "CI workflow injection",
        ] {
            assert!(
                RULES.iter().any(|(r, _)| r.category == category),
                "{category}"
            );
        }
    }

    #[test]
    fn each_class_is_found_in_typical_code() {
        let cases = [
            ("data = pickle.loads(request.data)", "py-pickle-loads"),
            ("cfg = yaml.load(text)", "py-yaml-load"),
            ("subprocess.run(cmd, shell=True)", "py-subprocess-shell"),
            ("el.innerHTML = userInput;", "js-inner-html"),
            ("h = hashlib.md5(pw).hexdigest()", "weak-hash-md5"),
            (
                "DocumentBuilderFactory dbf = DocumentBuilderFactory.newInstance();",
                "java-xxe",
            ),
            (
                r#"<script src="https://cdn.example/x.js"></script>"#,
                "html-script-without-sri",
            ),
            (
                "run: echo \"${{ github.event.issue.title }}\"",
                "gha-untrusted-expression",
            ),
            (
                "cur.execute(f\"SELECT * FROM t WHERE id={id}\")",
                "sql-string-building",
            ),
        ];
        for (code, rule) in cases {
            let ids: Vec<&str> = scan(code).iter().map(|f| f.id).collect();
            assert!(ids.contains(&rule), "{code}: {ids:?}");
        }
    }

    #[test]
    fn ordinary_and_hardened_code_is_quiet() {
        for code in [
            "data = json.loads(request.data)",
            "cfg = yaml.safe_load(text)",
            "subprocess.run([\"ls\", path])",
            "el.textContent = userInput;",
            "h = hashlib.sha256(data).hexdigest()",
            r#"<script src="https://cdn.example/x.js" integrity="sha384-abc" crossorigin="anonymous"></script>"#,
            "cur.execute(\"SELECT * FROM t WHERE id = ?\", (id,))",
            "const m = /a/.exec(text);",
        ] {
            assert_eq!(scan(code), Vec::new(), "{code}");
        }
    }
}
