// A profile directory comes only from `Home::profile(name)`, never from a
// bare path, so config cannot be read from an arbitrary directory.
use std::path::PathBuf;

use pmt_config::ProfileDir;

fn main() {
    let _profile = ProfileDir::from(PathBuf::from("/srv/pmt/profiles/work"));
}
