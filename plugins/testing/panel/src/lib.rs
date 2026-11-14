//! Test plugin: a dashboard plugin's backend (the `ui-panel` world). Every
//! backend request answers a forecast echoing the request; a request for
//! `/fail` is refused by the plugin itself.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/ui-panel"],
    world: "prometheus:ui-panel/ui-panel",
    generate_all,
});

use exports::prometheus::ui_panel::plugin::Guest;

struct Panel;

impl Guest for Panel {
    fn title() -> String {
        "Weather".to_owned()
    }

    fn render(_state: String) -> Result<String, String> {
        Ok("{}".to_owned())
    }

    fn on_action(action: String) -> Result<String, String> {
        if action.contains("\"path\":\"/fail\"") {
            return Err("no forecast today".to_owned());
        }
        Ok(format!(
            r#"{{"status":200,"body":{{"forecast":"sunny","request":{action}}}}}"#
        ))
    }
}

export!(Panel);
