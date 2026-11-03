//! Sample `delegation-router` plugin: keeps every subtask on the parent's
//! model.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::delegation_router::plugin::Guest;

struct Sample;

impl Guest for Sample {
    fn route(_task: String) -> Result<String, String> {
        Ok(r#"{"model":null}"#.to_owned())
    }
}

bindings::export!(Sample with_types_in bindings);
