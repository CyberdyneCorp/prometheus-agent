//! The audio ports the pipeline talks to: a [`Microphone`] that captures
//! 16 kHz mono PCM, a [`Player`] that plays clips in the order given, a
//! [`Transcriber`] and a [`Synth`]. Hosts wire the profile's speech
//! providers and this machine's audio tools behind them; tests use
//! fixtures.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::sync::mpsc;

/// A boxed future that can cross threads.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Captures audio.
pub trait Microphone: Send + Sync {
    /// The input devices it can record from (the first is used); empty
    /// when there is none.
    fn devices(&self) -> BoxFuture<'_, Vec<String>>;
    /// Starts recording from the first device.
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Capture>, String>>;
}

/// One running recording; dropping it stops the device.
pub trait Capture: Send {
    /// The next samples (16 kHz mono); `None` when the device stopped.
    fn read(&mut self) -> BoxFuture<'_, Option<Vec<i16>>>;
}

/// Something to play.
#[derive(Debug)]
pub enum Sound {
    /// An audio file (WAV, MP3, Ogg...).
    File(PathBuf),
    /// An audio file made for this clip, deleted once played.
    Scratch(PathBuf),
    /// Mono 16-bit samples at `rate` Hz.
    Pcm {
        /// Samples per second.
        rate: u32,
        /// The samples.
        samples: Vec<i16>,
    },
    /// Raw mono 16-bit little-endian PCM at `rate` Hz as a provider
    /// streams it.
    Stream {
        /// Samples per second.
        rate: u32,
        /// The chunks, in order; the stream ends when the sender drops.
        chunks: mpsc::Receiver<Vec<u8>>,
    },
}

/// A sound and what it is (`beep:start`, `beep:end`, or the sentence a
/// spoken clip says), for logs and test fixtures.
#[derive(Debug)]
pub struct Clip {
    /// What it is.
    pub label: String,
    /// The audio.
    pub sound: Sound,
}

/// Plays clips, one at a time.
pub trait Player: Send + Sync {
    /// Plays `clip` to its end.
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>>;
}

/// Speech-to-text for a recording.
pub trait Transcriber: Send + Sync {
    /// The transcript of `samples` (16 kHz mono).
    fn transcribe<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<String, String>>;
}

/// Text-to-speech for one sentence.
pub trait Synth: Send + Sync {
    /// The audio of `sentence`: a file, or a stream for providers that
    /// send chunked PCM.
    fn synthesize<'a>(&'a self, sentence: &'a str) -> BoxFuture<'a, Result<Sound, String>>;
}
