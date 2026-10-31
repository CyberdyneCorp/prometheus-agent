// A profile's store is located only through a loaded `ProfileScope`:
// neither a path nor a profile directory will do.
use pmt_config::ProfileDir;
use pmt_store::StoreLocation;

fn from_profile_dir(profile: &ProfileDir) -> StoreLocation {
    StoreLocation::of(profile)
}

fn main() {
    let path = std::path::Path::new("/srv/pmt/profiles/work");
    let _from_path = StoreLocation::of(path);
}
