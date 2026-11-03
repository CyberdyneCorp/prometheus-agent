//! Test plugin: tools that loop forever, exhaust memory or trap.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/messaging", "../../../wit/tool"],
    world: "prometheus:tool/tool",
    generate_all,
});

use exports::prometheus::tool::plugin::{Guest, ToolSpec};

struct Spin;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: format!("test tool {name}"),
        parameters: r#"{"type":"object"}"#.to_owned(),
    }
}

impl Guest for Spin {
    fn tools() -> Vec<ToolSpec> {
        ["spin", "grow", "trap", "ok"].into_iter().map(spec).collect()
    }

    fn call(name: String, _arguments: String) -> Result<String, String> {
        match name.as_str() {
            "spin" => loop {
                core::hint::black_box(());
            },
            "grow" => {
                let mut held: Vec<Vec<u8>> = Vec::new();
                loop {
                    held.push(vec![1u8; 1 << 20]);
                    core::hint::black_box(&held);
                }
            }
            "trap" => unreachable!("the trap tool traps"),
            "ok" => Ok("fine".to_owned()),
            other => Err(format!("no tool {other}")),
        }
    }
}

export!(Spin);
