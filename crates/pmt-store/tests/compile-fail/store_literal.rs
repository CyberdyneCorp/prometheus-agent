// A store location cannot be assembled by hand to point at any directory.
use pmt_store::StoreLocation;

fn main() {
    let path = std::path::Path::new("/srv/pmt/profiles/work");
    let _literal = StoreLocation {
        database: path.join("state.db"),
        blobs: path.join("blobs"),
    };
}
