// A scope cannot be assembled by hand: its fields are private.
use pmt_config::ProfileScope;

fn main() {
    let _scope = ProfileScope { inner: todo!() };
}
