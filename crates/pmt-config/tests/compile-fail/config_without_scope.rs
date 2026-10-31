// The effective configuration is read through `ProfileScope::config()`;
// the file loader is not public, and code that needs config takes a scope.
use pmt_config::{Config, Overrides, ProfileDir, ProfileScope};

fn needs_config(scope: &ProfileScope) -> &Config {
    scope.config()
}

fn bypass(profile: &ProfileDir) {
    let _layered = profile.config_file().load(&Overrides::default());
}

fn main() {
    let path = std::path::Path::new("/srv/pmt/profiles/work");
    let _config = needs_config(path);
}
