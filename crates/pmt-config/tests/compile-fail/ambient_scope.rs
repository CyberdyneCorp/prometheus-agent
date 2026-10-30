// There is no process-global "current profile" to fall back on.
use pmt_config::ProfileScope;

fn main() {
    let _current = ProfileScope::current();
    let _default = ProfileScope::default();
}
