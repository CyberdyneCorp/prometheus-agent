//! Unpacks a verified runtime archive into a staging directory. Both
//! readers refuse entries that would land outside the directory (absolute
//! paths, `..`), and keep Unix permissions so executables stay executable.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use flate2::read::GzDecoder;

use super::index::Format;

/// Unpacks `archive` (a `format` file) into `dest`, which must exist; a
/// bare executable is copied to `dest/<exe>` and made executable.
///
/// # Errors
///
/// A one-line reason when the archive is unreadable.
pub fn unpack(archive: &Path, format: Format, dest: &Path, exe: &str) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    match format {
        Format::Binary => place_binary(archive, &dest.join(exe)),
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

/// Copies a bare executable into place, executable on Unix; its path
/// must stay below the destination.
fn place_binary(archive: &Path, target: &Path) -> Result<(), String> {
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("the executable path leaves the runtime directory".to_owned());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::copy(archive, target).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
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
        unpack(&archive, Format::Zip, &dest, "top/bin/tool").unwrap();
        assert_eq!(std::fs::read(dest.join("top/bin/tool")).unwrap(), b"tool");
    }

    #[test]
    fn a_bare_executable_lands_at_its_exe_path() {
        let dir = tempfile::tempdir().unwrap();
        let download = dir.path().join("lightpanda-aarch64-macos");
        std::fs::write(&download, b"\x7fELF").unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        unpack(&download, Format::Binary, &dest, "lightpanda").unwrap();
        assert_eq!(std::fs::read(dest.join("lightpanda")).unwrap(), b"\x7fELF");
        assert!(unpack(&download, Format::Binary, &dest, "../escape").is_err());
    }

    #[test]
    fn a_corrupt_archive_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        std::fs::write(&archive, b"not gzip").unwrap();
        assert!(unpack(&archive, Format::TarGz, dir.path(), "x").is_err());
    }
}
