//! Sample `platform-adapter` plugin: a loopback platform that answers
//! every send with a message id and receives nothing.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::platform_adapter::plugin::{Guest, HttpRequest, HttpResponse};

struct Sample;

impl Guest for Sample {
    fn descriptor() -> String {
        r#"{"platform":"sample","max_message_length":4096}"#.to_owned()
    }

    fn start(_options: String) -> Result<(), String> {
        Ok(())
    }

    fn stop() {}

    fn poll() -> Result<String, String> {
        Ok("[]".to_owned())
    }

    fn send(_message: String) -> Result<String, String> {
        Ok("sample-1".to_owned())
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

bindings::export!(Sample with_types_in bindings);
