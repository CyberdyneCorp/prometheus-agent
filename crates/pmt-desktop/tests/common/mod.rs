//! Test doubles shared by the shell's integration tests.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_desktop::Native;

/// A [`Native`] that does nothing.
pub(crate) struct Silent;

impl Native for Silent {
    fn open_url(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
    fn open_path(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn pick(&self, _directory: bool, _title: Option<&str>) -> Option<PathBuf> {
        None
    }
    fn notify(&self, _title: &str, _body: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_badge(&self, _window: &str, _count: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    fn register_hotkey(&self, _chord: &str) -> Result<(), String> {
        Ok(())
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
}

/// A log writer into a shared buffer.
pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
