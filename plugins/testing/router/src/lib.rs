//! Test plugin: a delegation router. A task whose goal contains `refuse`
//! is refused; a task naming `profile` (a router parameter) runs in that
//! profile on `openrouter/test-model`; every other task keeps the normal
//! choice.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/delegation-router"],
    world: "prometheus:delegation-router/delegation-router",
    generate_all,
});

use exports::prometheus::delegation_router::plugin::Guest;

struct Router;

/// The string value of `"key":"..."` in `json` (test tasks hold no
/// escaped quotes).
fn field(json: &str, key: &str) -> Option<String> {
    let start = json.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let rest = &json[start..];
    rest.find('"').map(|end| rest[..end].to_owned())
}

impl Guest for Router {
    fn route(task: String) -> Result<String, String> {
        let goal = field(&task, "goal").unwrap_or_default();
        if goal.contains("refuse") {
            return Err(format!("no tier fits '{goal}'"));
        }
        match field(&task, "profile") {
            Some(profile) => Ok(format!(
                r#"{{"profile":"{profile}","model":"openrouter/test-model"}}"#
            )),
            None => Ok("{}".to_owned()),
        }
    }
}

export!(Router);
