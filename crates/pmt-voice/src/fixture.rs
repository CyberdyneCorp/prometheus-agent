//! Recorded audio instead of a sound card, for tests and debug-build test
//! seams: a [`FixtureMicrophone`] plays WAV files from a directory as if
//! they were spoken into a microphone, and a [`FixturePlayer`] logs what
//! would have been played.
//!
//! Directory layout: `device` names the input device (no file: no
//! device); `mic-*.wav` are the recordings, used once each in name order
//! (renamed to `*.used`, so the order holds across processes), followed
//! by endless room silence; `played.log` gets one line per clip played.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::device::{BoxFuture, Capture, Clip, Microphone, Player, Sound};
use crate::pcm;

/// Samples handed out per read (100 ms).
const CHUNK: usize = 1_600;

/// WAV fixtures as a microphone.
#[derive(Debug, Clone)]
pub struct FixtureMicrophone {
    dir: PathBuf,
}

impl FixtureMicrophone {
    /// The microphone of the fixtures in `dir`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The next unused recording, marked used.
    fn next_recording(&self) -> Result<Vec<i16>, String> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("mic-"))
                    && path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            })
            .collect();
        names.sort();
        let Some(path) = names.first() else {
            return Ok(Vec::new());
        };
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let mut used = path.clone().into_os_string();
        used.push(".used");
        std::fs::rename(path, used).map_err(|e| e.to_string())?;
        match pcm::read_wav(&bytes) {
            Some((pcm::RATE, samples)) => Ok(samples),
            _ => Err(format!("{}: not 16 kHz 16-bit PCM", path.display())),
        }
    }
}

impl Microphone for FixtureMicrophone {
    fn devices(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            std::fs::read_to_string(self.dir.join("device"))
                .map(|name| vec![name.trim().to_owned()])
                .unwrap_or_default()
        })
    }

    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Capture>, String>> {
        Box::pin(async move {
            let samples = self.next_recording()?;
            Ok(Box::new(Fixture { samples, at: 0 }) as Box<dyn Capture>)
        })
    }
}

struct Fixture {
    samples: Vec<i16>,
    at: usize,
}

impl Capture for Fixture {
    fn read(&mut self) -> BoxFuture<'_, Option<Vec<i16>>> {
        Box::pin(async move {
            // Room silence after the recording; yield so a recording that
            // waits for silence never starves other tasks.
            tokio::task::yield_now().await;
            let end = (self.at + CHUNK).min(self.samples.len());
            let mut chunk = self.samples[self.at..end].to_vec();
            self.at = end;
            chunk.resize(CHUNK, 0);
            Some(chunk)
        })
    }
}

/// Logs clips to `played.log` instead of playing them.
#[derive(Debug, Clone)]
pub struct FixturePlayer {
    dir: PathBuf,
}

impl FixturePlayer {
    /// The player logging into `dir`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }
}

impl Player for FixturePlayer {
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let kind = match clip.sound {
                Sound::File(path) => format!("file {}", path.display()),
                Sound::Scratch(path) => {
                    let _ = std::fs::remove_file(&path);
                    "scratch file".to_owned()
                }
                Sound::Pcm { rate, samples } => format!("pcm {rate} Hz {} samples", samples.len()),
                Sound::Stream { rate, mut chunks } => {
                    let mut bytes = 0;
                    while let Some(chunk) = chunks.recv().await {
                        bytes += chunk.len();
                    }
                    format!("stream {rate} Hz {bytes} bytes")
                }
            };
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join("played.log"))
                .map_err(|e| e.to_string())?;
            writeln!(log, "{}\t{kind}", clip.label).map_err(|e| e.to_string())
        })
    }
}
