//! Trajectory file names and the compression of ended sessions
//! (spec: observability, Trajectory retention and size limits): a running
//! session writes `<profile>/trajectories/<session_id>.jsonl`; when it ends
//! the file becomes `<session_id>.jsonl.zst`.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

use pmt_core::private_fs;

/// The trajectories directory under a profile.
pub const DIR: &str = "trajectories";

const PLAIN: &str = ".jsonl";
const COMPRESSED: &str = ".jsonl.zst";

/// zstd level of ended trajectories: fast, and JSONL compresses well.
const LEVEL: i32 = 3;

/// `<dir>/<session_id>.jsonl`, the file of a running session.
#[must_use]
pub fn plain(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}{PLAIN}"))
}

/// `<dir>/<session_id>.jsonl.zst`, the file of an ended session.
#[must_use]
pub fn compressed(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}{COMPRESSED}"))
}

/// The session's trajectory file, plain or compressed, if one exists.
#[must_use]
pub fn find(dir: &Path, session_id: &str) -> Option<PathBuf> {
    [plain(dir, session_id), compressed(dir, session_id)]
        .into_iter()
        .find(|path| path.is_file())
}

/// The session id a trajectory file belongs to, from its name.
#[must_use]
pub fn session_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(COMPRESSED)
        .or_else(|| name.strip_suffix(PLAIN))
}

/// Removes both forms of the session's trajectory; whether one existed.
///
/// # Errors
///
/// Any I/O error other than a missing file.
pub fn remove(dir: &Path, session_id: &str) -> io::Result<bool> {
    let mut removed = false;
    for path in [plain(dir, session_id), compressed(dir, session_id)] {
        match fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(removed)
}

/// Reads a trajectory file, decompressing a `.zst` one.
///
/// # Errors
///
/// Any I/O or decompression error.
pub fn open(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.to_string_lossy().ends_with(COMPRESSED) {
        let decoder = zstd::stream::read::Decoder::new(file)?;
        Ok(Box::new(BufReader::new(decoder)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Compresses `<id>.jsonl` to `<id>.jsonl.zst` (0600, replaced atomically)
/// and removes the plain file. Returns the compressed path.
///
/// # Errors
///
/// Any I/O error; the plain file is kept then.
pub fn compress(plain_path: &Path) -> io::Result<PathBuf> {
    let target = PathBuf::from(format!("{}.zst", plain_path.to_string_lossy()));
    let mut source = File::open(plain_path)?;
    let temp = sibling_temp(&target);
    let written = (|| {
        let file = private_fs::new_file_options().open(&temp)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, LEVEL)?;
        io::copy(&mut source, &mut encoder)?;
        let file = encoder.finish()?;
        file.sync_all()?;
        fs::rename(&temp, &target)
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    fs::remove_file(plain_path)?;
    Ok(target)
}

/// Turns `<id>.jsonl.zst` back into `<id>.jsonl` (a reopened session keeps
/// appending to its trajectory). Returns the plain path.
///
/// # Errors
///
/// Any I/O or decompression error; the compressed file is kept then.
pub fn decompress(compressed_path: &Path) -> io::Result<PathBuf> {
    let name = compressed_path.to_string_lossy();
    let target = PathBuf::from(name.strip_suffix(".zst").unwrap_or(&name));
    let temp = sibling_temp(&target);
    let written = (|| {
        let mut decoder = zstd::stream::read::Decoder::new(File::open(compressed_path)?)?;
        let mut file = private_fs::new_file_options().open(&temp)?;
        io::copy(&mut decoder, &mut file)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, &target)
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    fs::remove_file(compressed_path)?;
    Ok(target)
}

fn sibling_temp(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let temp = PathBuf::from(name);
    let _ = fs::remove_file(&temp);
    temp
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::{compress, decompress, find, open, plain, remove, session_of};

    #[test]
    fn compression_round_trips_and_find_sees_either_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain(dir.path(), "ses_X");
        std::fs::write(&path, "{\"a\":1}\n").unwrap();
        let zst = compress(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(find(dir.path(), "ses_X").unwrap(), zst);
        assert_eq!(session_of(&zst), Some("ses_X"));
        let mut text = String::new();
        open(&zst).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "{\"a\":1}\n");
        assert_eq!(decompress(&zst).unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        assert!(remove(dir.path(), "ses_X").unwrap());
        assert!(find(dir.path(), "ses_X").is_none());
    }
}
