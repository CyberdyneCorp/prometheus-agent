//! A session's modes, models and config options as ACP shows them (spec:
//! acp-adapter, Edit-approval modes; Config options are validated; Model
//! switching through the daemon), built from the daemon's
//! `session.settings`, and the validation of an editor's changes.

use pmt_rpc::contracts::{ConfigureParams, EditMode, SessionSettings};
use serde_json::{Value, json};

use crate::frames::AcpError;

/// The config option ids an editor may set.
pub(crate) const OPTION_IDS: [&str; 3] = ["edit_approval_policy", "model", "reasoning_effort"];

fn mode_name(mode: EditMode) -> (&'static str, &'static str) {
    match mode {
        EditMode::Default => ("Ask before edits", "Every file edit asks first"),
        EditMode::AcceptEdits => (
            "Accept edits",
            "Edits inside the workspace run without asking",
        ),
        EditMode::DontAsk => (
            "Don't ask",
            "Every edit of this session runs without asking",
        ),
    }
}

/// The current edit mode (`default` for a session without one).
pub(crate) fn mode_of(settings: &SessionSettings) -> EditMode {
    settings.edit_mode.unwrap_or(EditMode::Default)
}

/// The `<provider>/<model>` id of the session's model.
pub(crate) fn model_id(settings: &SessionSettings) -> String {
    format!("{}/{}", settings.provider, settings.model)
}

/// The models a switch may pick, the current one first.
fn models_of(settings: &SessionSettings) -> Vec<String> {
    let current = model_id(settings);
    let mut models = vec![current.clone()];
    models.extend(settings.models.iter().filter(|m| **m != current).cloned());
    models
}

/// ACP `modes`.
pub(crate) fn modes(settings: &SessionSettings) -> Value {
    let available: Vec<Value> = EditMode::ALL
        .iter()
        .map(|mode| {
            let (name, description) = mode_name(*mode);
            json!({"id": mode.as_str(), "name": name, "description": description})
        })
        .collect();
    json!({"currentModeId": mode_of(settings).as_str(), "availableModes": available})
}

/// ACP `models`.
pub(crate) fn models(settings: &SessionSettings) -> Value {
    let available: Vec<Value> = models_of(settings)
        .into_iter()
        .map(|id| json!({"modelId": id, "name": id}))
        .collect();
    json!({"currentModelId": model_id(settings), "availableModels": available})
}

fn select(id: &str, name: &str, current: &str, values: &[String]) -> Value {
    let options: Vec<Value> = values
        .iter()
        .map(|v| json!({"value": v, "name": v}))
        .collect();
    json!({"id": id, "name": name, "type": "select", "currentValue": current, "options": options})
}

/// The values option `id` accepts.
fn allowed(settings: &SessionSettings, id: &str) -> Vec<String> {
    match id {
        "edit_approval_policy" => EditMode::ALL
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
        "model" => models_of(settings),
        _ => settings.reasoning_efforts.clone(),
    }
}

/// ACP `configOptions`.
pub(crate) fn config_options(settings: &SessionSettings) -> Value {
    json!([
        select(
            "edit_approval_policy",
            "Edit approval",
            mode_of(settings).as_str(),
            &allowed(settings, "edit_approval_policy"),
        ),
        select(
            "model",
            "Model",
            &model_id(settings),
            &allowed(settings, "model")
        ),
        select(
            "reasoning_effort",
            "Reasoning effort",
            &settings.reasoning_effort,
            &allowed(settings, "reasoning_effort"),
        ),
    ])
}

/// The `session.configure` of `set_config_option(id, value)`.
///
/// # Errors
///
/// `-32602 unknown config option <id>`, or `-32602` naming the allowed
/// values.
pub(crate) fn change(
    settings: &SessionSettings,
    id: &str,
    value: &str,
) -> Result<ConfigureParams, AcpError> {
    if !OPTION_IDS.contains(&id) {
        return Err(AcpError::invalid(format!("unknown config option {id}")));
    }
    let values = allowed(settings, id);
    if !values.iter().any(|v| v == value) {
        return Err(AcpError::invalid(format!(
            "invalid value {value} for {id}; allowed: {}",
            values.join(", ")
        )));
    }
    let mut params = ConfigureParams {
        session_id: settings.session_id.clone(),
        ..ConfigureParams::default()
    };
    match id {
        "edit_approval_policy" => params.edit_mode = EditMode::parse(value),
        "model" => params.model = Some(value.to_owned()),
        _ => params.reasoning_effort = Some(value.to_owned()),
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::{EditMode, SessionSettings};

    use super::{change, config_options, modes};

    fn settings() -> SessionSettings {
        SessionSettings {
            session_id: "ses_x".into(),
            provider: "mock".into(),
            model: "mock".into(),
            models: vec!["mock/other".into()],
            reasoning_effort: "medium".into(),
            reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
            edit_mode: Some(EditMode::Default),
            cwd: "/work".into(),
            busy: false,
            title: None,
        }
    }

    /// Scenario: Unknown option id.
    #[test]
    fn an_unknown_option_or_value_is_refused_naming_what_is_allowed() {
        let err = change(&settings(), "temperature", "0.2").unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (-32602, "unknown config option temperature")
        );
        let err = change(&settings(), "reasoning_effort", "loud").unwrap_err();
        assert_eq!(
            err.message,
            "invalid value loud for reasoning_effort; allowed: low, medium, high"
        );
        let ok = change(&settings(), "edit_approval_policy", "accept_edits").unwrap();
        assert_eq!(ok.edit_mode, Some(EditMode::AcceptEdits));
    }

    #[test]
    fn new_sessions_start_in_default_mode_with_three_options() {
        assert_eq!(modes(&settings())["currentModeId"], "default");
        let options = config_options(&settings());
        let ids: Vec<&str> = options
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["edit_approval_policy", "model", "reasoning_effort"]);
    }
}
