//! Test plugin: an adapter built against a `platform-adapter` world that
//! lacks `send` (`wit/world.wit`), which the host refuses to load.

wit_bindgen::generate!({
    path: ["../../../wit/host", "wit"],
    world: "prometheus:platform-adapter/platform-adapter",
    generate_all,
});

use exports::prometheus::platform_adapter::plugin::{Guest, HttpRequest, HttpResponse};

struct NoSend;

impl Guest for NoSend {
    fn descriptor() -> String {
        r#"{"platform":"nosend","max_message_length":100}"#.to_owned()
    }

    fn start(_options: String) -> Result<(), String> {
        Ok(())
    }

    fn stop() {}

    fn poll() -> Result<String, String> {
        Ok("[]".to_owned())
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(format!(r#"{{"name":"{chat}","chat_type":"dm"}}"#))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn authorize_sender(_inbound: String) -> String {
        "unknown".to_owned()
    }
}

export!(NoSend);
