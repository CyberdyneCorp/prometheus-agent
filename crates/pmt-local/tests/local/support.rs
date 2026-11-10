//! Fixture builders shared by the tests.

use std::path::Path;

use pmt_update::manifest::sha256_hex;

/// A `.tar.gz` holding `files` (path, contents).
pub(crate) fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Lower-case hex SHA-256.
pub(crate) fn sha(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// A reqwest client for the loopback fixtures.
pub(crate) fn client() -> reqwest::Client {
    pmt_local::server::detect::loopback_client().unwrap()
}

/// Writes `daemon.toml` into `home`.
pub(crate) fn daemon_toml(home: &Path, text: &str) {
    std::fs::write(home.join("daemon.toml"), text).unwrap();
}

/// A temp dir under the target directory.
pub(crate) fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
