//! The `computer_use` actions (spec: computer-use, `computer_use` tool
//! actions): each one becomes driver calls against the sticky target, the
//! window the last capture or `focus_app` chose. Input is always addressed
//! to that window's process and id, and is delivered in the background
//! (no cursor move, no raise, no desktop switch) unless the call asks for
//! `delivery_mode = "foreground"` — a separately approved variant.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::capture;
use super::driver::{Driver, DriverReply};
use super::session::{State, Target, call};
use crate::result::{Status, ToolResult};

/// Every action, in the schema's order.
pub const ACTIONS: &[&str] = &[
    "capture",
    "click",
    "double_click",
    "right_click",
    "middle_click",
    "drag",
    "scroll",
    "type",
    "key",
    "set_value",
    "wait",
    "list_apps",
    "list_windows",
    "focus_app",
];

/// Actions that send input to the sticky target.
const INPUT: &[&str] = &[
    "click",
    "double_click",
    "right_click",
    "middle_click",
    "drag",
    "scroll",
    "type",
    "key",
    "set_value",
];

/// The longest `wait`.
const MAX_WAIT_SECS: f64 = 30.0;

/// Modifier key names the driver's `hotkey` takes.
const MODIFIERS: &[&str] = &["cmd", "shift", "option", "ctrl", "fn", "win"];

/// One line for the approval prompt.
#[must_use]
pub fn summary(action: &str, arguments: &Value) -> String {
    let at = match (
        arguments["element"].as_i64(),
        arguments["x"].as_i64(),
        arguments["y"].as_i64(),
    ) {
        (Some(element), ..) => format!(" element {element}"),
        (None, Some(x), Some(y)) => format!(" at ({x}, {y})"),
        _ => String::new(),
    };
    let held = arguments["modifiers"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("+")
        })
        .filter(|m| !m.is_empty())
        .map_or_else(String::new, |m| format!(" holding {m}"));
    match action {
        "type" => {
            let text = arguments["text"].as_str().unwrap_or_default();
            let shown: String = text.chars().take(60).collect();
            let more = if text.chars().count() > 60 { "..." } else { "" };
            format!("type {shown:?}{more}")
        }
        "key" => format!("press {}", super::safety::pressed(arguments)),
        "focus_app" => format!("focus {}", arguments["app"].as_str().unwrap_or_default()),
        "set_value" => format!(
            "set{at} to {:?}",
            arguments["value"].as_str().unwrap_or_default()
        ),
        "scroll" => format!(
            "scroll {}{at}",
            arguments["direction"].as_str().unwrap_or("down")
        ),
        other => format!("{other}{at}{held}"),
    }
}

/// A finished action's result: the driver's verdict as JSON text.
fn verdict(action: &str, reply: &DriverReply) -> ToolResult {
    let mut body = json!({"ok": !reply.is_error, "action": action});
    let message = reply.message();
    if !message.is_empty() {
        body["message"] = json!(message);
    }
    if let Value::Object(fields) = &reply.structured {
        for key in ["verified", "effect", "escalation", "code", "delivery_mode"] {
            if let Some(value) = fields.get(key) {
                body[key] = value.clone();
            }
        }
    }
    let status = if reply.is_error {
        Status::Error
    } else {
        Status::Ok
    };
    ToolResult::text(status, body.to_string())
}

/// A refusal before any input was sent.
pub(super) fn refused(message: impl Into<String>) -> ToolResult {
    ToolResult::error(message.into())
}

/// `wait`: pauses up to 30 seconds without touching the driver.
pub(super) async fn wait(arguments: &Value) -> ToolResult {
    let seconds = arguments["seconds"]
        .as_f64()
        .unwrap_or(1.0)
        .clamp(0.0, MAX_WAIT_SECS);
    tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
    ToolResult::ok(json!({"ok": true, "action": "wait", "seconds": seconds}).to_string())
}

/// Context of one action.
pub(super) struct Act<'a> {
    pub driver: &'a Arc<dyn Driver>,
    pub state: &'a mut State,
    pub label: &'a str,
    pub arguments: &'a Value,
}

impl Act<'_> {
    /// Runs a driver-backed action.
    pub(super) async fn run(&mut self, action: &str) -> Result<ToolResult, String> {
        match action {
            "list_apps" => self.listing("list_apps", "apps").await,
            "list_windows" => self.listing("list_windows", "windows").await,
            "focus_app" => self.focus_app().await,
            _ if INPUT.contains(&action) => self.input(action).await,
            _ => Ok(refused(format!("unknown computer_use action '{action}'"))),
        }
    }

    async fn listing(&self, tool: &str, key: &str) -> Result<ToolResult, String> {
        let mut arguments = json!({"session": self.label});
        if tool == "list_windows" {
            arguments["on_screen_only"] = json!(true);
        }
        let reply = call(self.driver, tool, arguments).await?;
        if reply.is_error {
            return Ok(verdict(tool, &reply));
        }
        let items = reply.field(key).cloned().unwrap_or_else(|| json!([]));
        let count = items.as_array().map_or(0, Vec::len);
        Ok(ToolResult::ok(
            json!({key: items, "count": count}).to_string(),
        ))
    }

    /// Selects an application's frontmost window as the target, raising
    /// it only with `raise_window`.
    async fn focus_app(&mut self) -> Result<ToolResult, String> {
        let Some(app) = self.arguments["app"].as_str().filter(|a| !a.is_empty()) else {
            return Ok(refused("focus_app requires `app`"));
        };
        let Some(target) = self.find(Some(app), None).await? else {
            return Ok(refused(format!(
                "no on-screen window found for app '{app}'"
            )));
        };
        self.select(target.clone());
        if self.arguments["raise_window"].as_bool() != Some(true) {
            return Ok(ToolResult::ok(
                json!({
                    "ok": true,
                    "action": "focus_app",
                    "message": format!("targeted {} (pid {}, window {}) without raising it", target.app, target.pid, target.window_id),
                })
                .to_string(),
            ));
        }
        let reply = call(
            self.driver,
            "bring_to_front",
            json!({"pid": target.pid, "window_id": target.window_id}),
        )
        .await?;
        Ok(verdict("focus_app", &reply))
    }

    /// The window `app` / `window` select (frontmost first).
    pub(super) async fn find(
        &self,
        app: Option<&str>,
        window: Option<&str>,
    ) -> Result<Option<Target>, String> {
        let reply = call(
            self.driver,
            "list_windows",
            json!({"session": self.label, "on_screen_only": true}),
        )
        .await?;
        if reply.is_error {
            return Err(format!("list_windows failed: {}", reply.message()));
        }
        Ok(capture::select_window(&reply, app, window))
    }

    /// Makes `target` the sticky target; the old capture's elements no
    /// longer apply.
    pub(super) fn select(&mut self, target: Target) {
        if self.state.target.as_ref() != Some(&target) {
            self.state.elements.clear();
        }
        self.state.target = Some(target);
    }

    async fn input(&mut self, action: &str) -> Result<ToolResult, String> {
        let app = self.arguments["app"].as_str().filter(|a| !a.is_empty());
        let window = self.arguments["window"].as_str().filter(|w| !w.is_empty());
        if app.is_some() || window.is_some() {
            match self.find(app, window).await? {
                Some(target) => self.select(target),
                None => {
                    return Ok(refused(format!(
                        "no on-screen window matches app={app:?} window={window:?}; call list_windows"
                    )));
                }
            }
        }
        let Some(target) = self.state.target.clone() else {
            return Ok(refused(
                "no target window: call computer_use(action=\"capture\") first",
            ));
        };
        for key in ["element", "to_element"] {
            if let Some(index) = self.arguments[key].as_i64()
                && !self.state.elements.contains_key(&index)
            {
                return Ok(refused(format!("stale element {index}; capture again")));
            }
        }
        let (tool, arguments) = match self.driver_call(action, &target) {
            Ok(call) => call,
            Err(why) => return Ok(refused(why)),
        };
        if arguments.contains_key("delivery_mode")
            && self.arguments["bring_to_front"].as_bool() == Some(true)
        {
            let front = json!({"pid": target.pid, "window_id": target.window_id});
            let fronted = call(self.driver, "bring_to_front", front).await?;
            if fronted.is_error {
                return Ok(verdict(action, &fronted));
            }
        }
        let reply = call(self.driver, tool, Value::Object(arguments)).await?;
        Ok(verdict(action, &reply))
    }

    /// The driver tool and arguments of an input action.
    fn driver_call(
        &self,
        action: &str,
        target: &Target,
    ) -> Result<(&'static str, Map<String, Value>), String> {
        let a = self.arguments;
        let mut out = Map::new();
        out.insert("session".into(), json!(self.label));
        out.insert("pid".into(), json!(target.pid));
        out.insert("window_id".into(), json!(target.window_id));
        if a["delivery_mode"].as_str() == Some("foreground") {
            out.insert("delivery_mode".into(), json!("foreground"));
        }
        // Modifiers held during a pointer action (cmd-click) go to the
        // driver as they are; for `key` they are part of the combination
        // (so the hard block saw them), and `type` takes none.
        let modifiers = a["modifiers"].as_array().filter(|m| !m.is_empty());
        match (action, modifiers) {
            ("key", _) | (_, None) => {}
            ("type", Some(_)) => {
                return Err("`modifiers` does not apply to type; press a combination with action=\"key\", keys=\"cmd+a\"".to_owned());
            }
            (_, Some(modifiers)) => {
                out.insert("modifier".into(), Value::Array(modifiers.clone()));
            }
        }
        let tool = match action {
            "click" | "double_click" | "right_click" | "middle_click" => {
                self.point(&mut out, "element", "x", "y", "")?;
                let button = match action {
                    "right_click" => "right",
                    "middle_click" => "middle",
                    _ => a["button"].as_str().unwrap_or("left"),
                };
                out.insert("button".into(), json!(button));
                if action == "double_click" {
                    "double_click"
                } else {
                    "click"
                }
            }
            "drag" => {
                self.point(&mut out, "element", "x", "y", "from_")?;
                self.point(&mut out, "to_element", "to_x", "to_y", "to_")?;
                "drag"
            }
            "scroll" => {
                let _ = self.point(&mut out, "element", "x", "y", "");
                out.insert(
                    "direction".into(),
                    json!(a["direction"].as_str().unwrap_or("down")),
                );
                out.insert(
                    "amount".into(),
                    json!(a["amount"].as_i64().unwrap_or(3).clamp(1, 50)),
                );
                "scroll"
            }
            "type" => {
                out.insert(
                    "text".into(),
                    json!(a["text"].as_str().ok_or("type requires `text`")?),
                );
                "type_text"
            }
            "key" => return key_call(&super::safety::pressed(a), out),
            "set_value" => {
                let value = a["value"].as_str().ok_or("set_value requires `value`")?;
                if a["element"].as_i64().is_none() {
                    return Err("set_value requires `element`".to_owned());
                }
                self.point(&mut out, "element", "x", "y", "")?;
                out.insert("value".into(), json!(value));
                "set_value"
            }
            other => return Err(format!("unknown computer_use action '{other}'")),
        };
        Ok((tool, out))
    }

    /// Adds an element (with its token) or coordinates under `prefix`.
    fn point(
        &self,
        out: &mut Map<String, Value>,
        element: &str,
        x: &str,
        y: &str,
        prefix: &str,
    ) -> Result<(), String> {
        let a = self.arguments;
        if let Some(index) = a[element].as_i64() {
            let name = if prefix.is_empty() {
                "element_index".to_owned()
            } else {
                format!("{prefix}element")
            };
            out.insert(name, json!(index));
            if prefix.is_empty()
                && let Some(Some(token)) = self.state.elements.get(&index)
            {
                out.insert("element_token".into(), json!(token));
            }
            return Ok(());
        }
        match (a[x].as_i64(), a[y].as_i64()) {
            (Some(x), Some(y)) => {
                out.insert(format!("{prefix}x"), json!(x));
                out.insert(format!("{prefix}y"), json!(y));
                Ok(())
            }
            _ => Err(format!("this action needs `{element}` or `{x}`/`{y}`")),
        }
    }
}

/// `hotkey` for a combination with modifiers, `press_key` otherwise.
fn key_call(
    keys: &str,
    mut out: Map<String, Value>,
) -> Result<(&'static str, Map<String, Value>), String> {
    let parts = super::safety::combo_keys(keys);
    let (modifiers, rest): (Vec<String>, Vec<String>) = parts
        .into_iter()
        .partition(|k| MODIFIERS.contains(&k.as_str()));
    let Some(key) = rest.last() else {
        return Err(format!("could not read a key from '{keys}'"));
    };
    if modifiers.is_empty() {
        out.insert("key".into(), json!(key));
        return Ok(("press_key", out));
    }
    let mut all = modifiers;
    all.push(key.clone());
    out.insert("keys".into(), json!(all));
    Ok(("hotkey", out))
}
