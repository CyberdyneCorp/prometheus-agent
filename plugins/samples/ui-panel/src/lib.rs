//! Sample `ui-panel` plugin: a panel that renders its state back.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::ui_panel::plugin::Guest;

struct Sample;

impl Guest for Sample {
    fn title() -> String {
        "Sample".to_owned()
    }

    fn render(state: String) -> Result<String, String> {
        Ok(format!(r#"{{"type":"text","state":{state}}}"#))
    }

    fn on_action(action: String) -> Result<String, String> {
        Ok(action)
    }
}

bindings::export!(Sample with_types_in bindings);
