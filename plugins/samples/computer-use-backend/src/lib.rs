//! Sample `computer-use-backend` plugin: an in-memory desktop with one
//! window and two buttons. A runtime's id is `<mode>:<session label>`, and
//! the window title names the runtime that captured it, so a session can
//! see which permission mode it is driving. A real backend would start a
//! remote desktop session in `launch` and forward each call to it.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::computer_use_backend::plugin::{
    Guest, Image, Reply, RuntimeSpec,
};

/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
    0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// The driver tools the sample answers.
const TOOLS: &[&str] = &[
    "list_windows",
    "get_window_state",
    "click",
    "type_text",
    "press_key",
    "scroll",
];

struct Sample;

fn reply(structured: String, images: Vec<Image>) -> Reply {
    Reply {
        is_error: false,
        text: structured.clone(),
        structured,
        images,
    }
}

impl Guest for Sample {
    fn name() -> String {
        "sample-desktop".to_owned()
    }

    fn check() -> Result<String, String> {
        Ok("in-memory sample desktop".to_owned())
    }

    fn launch(spec: RuntimeSpec) -> Result<String, String> {
        if spec.mode == "bounded" && spec.manifest.is_none() {
            return Err("a bounded runtime needs its capability manifest".to_owned());
        }
        Ok(format!("{}:{}", spec.mode, spec.session_label))
    }

    fn tools(_runtime: String) -> Vec<String> {
        TOOLS.iter().map(|t| (*t).to_owned()).collect()
    }

    fn call(runtime: String, tool: String, _arguments: String) -> Result<Reply, String> {
        Ok(match tool.as_str() {
            "list_windows" => reply(
                format!(r#"{{"windows":[{{"pid":11,"window_id":101,"app_name":"Sample App","title":"{runtime}","z_index":1}}]}}"#),
                Vec::new(),
            ),
            "get_window_state" => reply(
                format!(r#"{{"elements":[{{"element_index":1,"role":"AXButton","label":"Button 1","frame":{{"x":1,"y":1,"w":10,"h":10}}}},{{"element_index":2,"role":"AXButton","label":"Button 2","frame":{{"x":2,"y":2,"w":10,"h":10}}}}],"window_title":"{runtime}"}}"#),
                vec![Image {
                    mime: "image/png".to_owned(),
                    data: PNG.to_vec(),
                }],
            ),
            other if TOOLS.contains(&other) => {
                reply(format!(r#"{{"message":"{other} ok"}}"#), Vec::new())
            }
            other => Reply {
                is_error: true,
                text: format!("unknown tool {other}"),
                structured: String::new(),
                images: Vec::new(),
            },
        })
    }

    fn stop(_runtime: String) {}
}

bindings::export!(Sample with_types_in bindings);
