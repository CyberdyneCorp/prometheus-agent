//! Unpacks a verified runtime archive into a staging directory. Both
//! readers refuse entries that would land outside the directory (absolute
//! paths, `..`), and keep Unix permissions so executables stay executable.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use flate2::read::GzDecoder;

use super::index::Format;

/// Unpacks `archive` (a `format` file) into `dest`, which must exist.
///
/// # Errors
///
/// A one-line reason when the archive is unreadable.
pub(super) fn unpack(archive: &Path, format: Format, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    match format {
        Format::TarGz => {
            let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
            tar.set_preserve_permissions(true);
            tar.unpack(dest).map_err(|e| e.to_string())
        }
        Format::Zip => {
            let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| e.to_string())?;
            zip.extract(dest).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::unpack;
    use crate::runtimes::index::Format;

    #[test]
    fn zip_entries_are_unpacked_below_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o755);
        zip.start_file("top/bin/tool", options).unwrap();
        zip.write_all(b"tool").unwrap();
        zip.finish().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        unpack(&archive, Format::Zip, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("top/bin/tool")).unwrap(), b"tool");
    }

    #[test]
    fn a_corrupt_archive_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        std::fs::write(&archive, b"not gzip").unwrap();
        assert!(unpack(&archive, Format::TarGz, dir.path()).is_err());
    }
}
