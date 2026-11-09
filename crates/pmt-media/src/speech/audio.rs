//! Audio bytes: format sniffing, joining the chunks of one synthesis,
//! wrapping raw PCM as WAV and the ffmpeg conversions (Opus voice notes,
//! silence trimming before a cloud upload, 16 kHz mono normalization).
//!
//! ffmpeg is looked up on the `PATH` (or given explicitly); every caller
//! has a fallback when it is missing or fails: the voice note goes as a
//! plain audio file, the untrimmed audio is uploaded.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An audio container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// MPEG layer III.
    Mp3,
    /// Ogg (Opus or Vorbis).
    Ogg,
    /// RIFF WAVE.
    Wav,
    /// FLAC.
    Flac,
    /// AAC / M4A.
    M4a,
    /// Anything else.
    Other,
}

impl Format {
    /// Sniffs `bytes`.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Self {
        match bytes {
            [b'O', b'g', b'g', b'S', ..] => Self::Ogg,
            [
                b'R',
                b'I',
                b'F',
                b'F',
                _,
                _,
                _,
                _,
                b'W',
                b'A',
                b'V',
                b'E',
                ..,
            ] => Self::Wav,
            [b'f', b'L', b'a', b'C', ..] => Self::Flac,
            [b'I', b'D', b'3', ..] => Self::Mp3,
            [0xff, second, ..] if second & 0xe0 == 0xe0 => Self::Mp3,
            [_, _, _, _, b'f', b't', b'y', b'p', ..] => Self::M4a,
            _ => Self::Other,
        }
    }

    /// The file extension.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Other => "bin",
        }
    }

    /// The MIME type.
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Ogg => "audio/ogg",
            Self::Wav => "audio/wav",
            Self::Flac => "audio/flac",
            Self::M4a => "audio/mp4",
            Self::Other => "application/octet-stream",
        }
    }
}

/// The file extension of a MIME type (`bin` when unknown).
#[must_use]
pub fn extension_of(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/flac" => "flac",
        "audio/mp4" | "audio/aac" => "m4a",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

/// Whether a file is Opus in Ogg (a voice note as is).
#[must_use]
pub fn is_opus(bytes: &[u8]) -> bool {
    Format::sniff(bytes) == Format::Ogg && bytes.windows(8).take(512).any(|w| w == b"OpusHead")
}

/// 16-bit mono PCM at `rate` Hz as a WAV file.
#[must_use]
pub fn wav_from_pcm(pcm: &[u8], rate: u32) -> Vec<u8> {
    let data = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(pcm.len() + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// The `fmt ` chunk and samples of a WAV file.
fn wav_parts(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut at = 12;
    let (mut fmt, mut data) = (None, None);
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = bytes.get(at + 8..(at + 8 + size).min(bytes.len()))?;
        match id {
            b"fmt " => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    Some((fmt?, data?))
}

/// The length of a WAV file in seconds, from its header.
#[must_use]
pub fn wav_seconds(bytes: &[u8]) -> Option<f64> {
    if Format::sniff(bytes) != Format::Wav {
        return None;
    }
    let (fmt, data) = wav_parts(bytes)?;
    let byte_rate = u32::from_le_bytes(fmt.get(8..12)?.try_into().ok()?);
    #[allow(clippy::cast_precision_loss)]
    (byte_rate > 0).then(|| data.len() as f64 / f64::from(byte_rate))
}

/// The `Duration: HH:MM:SS.ss` ffmpeg prints for an input, in seconds.
#[must_use]
pub fn parse_duration(stderr: &str) -> Option<f64> {
    let stamp = stderr.split("Duration: ").nth(1)?.split(',').next()?;
    let mut parts = stamp.trim().split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3_600.0 + minutes * 60.0 + seconds)
}

/// One file of the chunks of a synthesis, in order: MP3 frames and Ogg
/// streams concatenate as they are (a chained Ogg file); WAV chunks are
/// merged under the first one's header.
///
/// # Errors
///
/// Chunks of different formats, or WAV chunks of different sample formats.
pub fn join(parts: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
    let mut parts = parts.into_iter();
    let Some(first) = parts.next() else {
        return Ok(Vec::new());
    };
    let format = Format::sniff(&first);
    let rest: Vec<Vec<u8>> = parts.collect();
    if rest.is_empty() {
        return Ok(first);
    }
    if rest.iter().any(|p| Format::sniff(p) != format) {
        return Err("the provider answered chunks in different audio formats".to_owned());
    }
    if format != Format::Wav {
        let mut out = first;
        for part in &rest {
            out.extend_from_slice(part);
        }
        return Ok(out);
    }
    let (fmt, data) = wav_parts(&first).ok_or("unreadable WAV chunk")?;
    let mut samples = data.to_vec();
    for part in &rest {
        let (other, data) = wav_parts(part).ok_or("unreadable WAV chunk")?;
        if other != fmt {
            return Err("WAV chunks of different sample formats".to_owned());
        }
        samples.extend_from_slice(data);
    }
    let rate = u32::from_le_bytes(
        fmt.get(4..8)
            .ok_or("short fmt chunk")?
            .try_into()
            .map_err(|_| "short fmt chunk")?,
    );
    Ok(wav_from_pcm(&samples, rate))
}

/// The pinned package whose wheel carries a static ffmpeg for every
/// platform, installed through the managed `uv` runtime.
pub const MANAGED_PACKAGE: &str = "imageio-ffmpeg==0.6.0";

/// Where ffmpeg comes from.
#[derive(Debug, Clone)]
enum Source {
    /// A known executable, or none.
    Fixed(Option<PathBuf>),
    /// The managed copy: `uv` resolves the pinned package's executable on
    /// first use; `fallback` (ffmpeg on the `PATH`) serves when it cannot.
    Managed {
        uv: PathBuf,
        fallback: Option<PathBuf>,
    },
}

/// Where ffmpeg is.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    source: Source,
    resolved: tokio::sync::OnceCell<Option<PathBuf>>,
}

impl Default for Ffmpeg {
    fn default() -> Self {
        Self::none()
    }
}

fn path_ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    pmt_tools::terminal::shell::find_executable("ffmpeg", &path, pathext.as_deref())
}

impl Ffmpeg {
    fn of(source: Source) -> Self {
        Self {
            source,
            resolved: tokio::sync::OnceCell::new(),
        }
    }

    /// ffmpeg on the `PATH`, if any.
    #[must_use]
    pub fn on_path() -> Self {
        Self::of(Source::Fixed(path_ffmpeg()))
    }

    /// The managed ffmpeg: [`MANAGED_PACKAGE`] through the `uv` at `uv`,
    /// else the one on the `PATH`.
    #[must_use]
    pub fn managed(uv: PathBuf) -> Self {
        Self::of(Source::Managed {
            uv,
            fallback: path_ffmpeg(),
        })
    }

    /// The ffmpeg at `exe`.
    #[must_use]
    pub fn at(exe: PathBuf) -> Self {
        Self::of(Source::Fixed(Some(exe)))
    }

    /// No ffmpeg.
    #[must_use]
    pub fn none() -> Self {
        Self::of(Source::Fixed(None))
    }

    /// The executable, resolved once.
    pub async fn exe(&self) -> Option<&PathBuf> {
        self.resolved
            .get_or_init(|| async {
                match &self.source {
                    Source::Fixed(exe) => exe.clone(),
                    Source::Managed { uv, fallback } => {
                        managed_exe(uv).await.or_else(|| fallback.clone())
                    }
                }
            })
            .await
            .as_ref()
    }

    /// Whether one is available.
    pub async fn available(&self) -> bool {
        self.exe().await.is_some()
    }

    async fn run(&self, args: &[&OsStr]) -> Result<(), String> {
        let exe = self.exe().await.ok_or("ffmpeg is not installed")?;
        let mut command = tokio::process::Command::new(exe);
        command
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_mins(2), command.output())
            .await
            .map_err(|_| "ffmpeg timed out".to_owned())?
            .map_err(|e| format!("ffmpeg could not start: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("ffmpeg failed: {}", tail(&stderr)))
        }
    }

    /// The length of the audio at `input` in seconds, when ffmpeg can
    /// tell.
    pub async fn duration(&self, input: &Path) -> Option<f64> {
        let exe = self.exe().await?;
        let mut command = tokio::process::Command::new(exe);
        // Without an output ffmpeg describes the input and exits non-zero.
        command
            .arg("-hide_banner")
            .arg("-i")
            .arg(input)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .ok()?
            .ok()?;
        parse_duration(&String::from_utf8_lossy(&output.stderr))
    }

    /// `input` as Opus in Ogg at `output` (a voice note).
    ///
    /// # Errors
    ///
    /// ffmpeg is missing or failed.
    pub async fn to_opus(&self, input: &Path, output: &Path) -> Result<(), String> {
        self.run(&[
            OsStr::new("-i"),
            input.as_os_str(),
            OsStr::new("-vn"),
            OsStr::new("-c:a"),
            OsStr::new("libopus"),
            OsStr::new("-b:a"),
            OsStr::new("48k"),
            output.as_os_str(),
        ])
        .await
    }

    /// `input` with its long pauses shortened to `keep_ms`, silence being
    /// below `threshold_db`.
    ///
    /// # Errors
    ///
    /// ffmpeg is missing or failed.
    pub async fn trim_silence(
        &self,
        input: &Path,
        output: &Path,
        threshold_db: i32,
        keep_ms: u64,
    ) -> Result<(), String> {
        #[allow(clippy::cast_precision_loss)]
        let keep = keep_ms as f64 / 1_000.0;
        let filter = format!(
            "silenceremove=start_periods=1:start_threshold={threshold_db}dB:stop_periods=-1:stop_duration={keep}:stop_threshold={threshold_db}dB:stop_silence={keep}"
        );
        self.run(&[
            OsStr::new("-i"),
            input.as_os_str(),
            OsStr::new("-af"),
            OsStr::new(&filter),
            output.as_os_str(),
        ])
        .await
    }

    /// `input` as 16 kHz mono WAV.
    ///
    /// # Errors
    ///
    /// ffmpeg is missing or failed.
    pub async fn normalize(&self, input: &Path, output: &Path) -> Result<(), String> {
        self.run(&[
            OsStr::new("-i"),
            input.as_os_str(),
            OsStr::new("-ar"),
            OsStr::new("16000"),
            OsStr::new("-ac"),
            OsStr::new("1"),
            output.as_os_str(),
        ])
        .await
    }
}

/// The executable of [`MANAGED_PACKAGE`] installed by `uv` (downloaded
/// once into uv's cache), or `None` when it cannot be had.
async fn managed_exe(uv: &Path) -> Option<PathBuf> {
    let mut command = tokio::process::Command::new(uv);
    command
        .args([
            "run",
            "--no-project",
            "--quiet",
            "--with",
            MANAGED_PACKAGE,
            "python",
            "-c",
        ])
        .arg("import sys, imageio_ffmpeg; sys.stdout.write(imageio_ffmpeg.get_ffmpeg_exe())")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_mins(5), command.output())
        .await
        .ok()?
        .ok()?;
    let exe = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (output.status.success() && exe.is_file()).then_some(exe)
}

/// The last lines of a command's stderr, for errors.
#[must_use]
pub fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    let start = lines.len().saturating_sub(5);
    lines[start..].join(" | ").chars().take(600).collect()
}

/// A voice-note copy of the audio at `path`: itself when it is Opus/Ogg,
/// else an Opus conversion beside it; `None` when it cannot be made (the
/// caller sends a plain audio attachment).
pub async fn voice_note(ffmpeg: &Ffmpeg, path: &Path) -> Option<PathBuf> {
    let head = tokio::fs::read(path).await.ok()?;
    if is_opus(&head) {
        return Some(path.to_path_buf());
    }
    let out = path.with_extension("voice.ogg");
    match ffmpeg.to_opus(path, &out).await {
        Ok(()) => Some(out),
        Err(why) => {
            tracing::info!(file = %path.display(), %why, "voice note conversion failed; sending audio");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_sniffed() {
        assert_eq!(Format::sniff(b"OggS\0\x02"), Format::Ogg);
        assert_eq!(Format::sniff(b"ID3\x04"), Format::Mp3);
        assert_eq!(Format::sniff(&[0xff, 0xfb, 0x90]), Format::Mp3);
        assert_eq!(Format::sniff(&wav_from_pcm(&[0, 0], 24_000)), Format::Wav);
        let mut opus = b"OggS".to_vec();
        opus.extend_from_slice(&[0; 24]);
        opus.extend_from_slice(b"OpusHead");
        assert!(is_opus(&opus));
        assert!(!is_opus(b"OggS vorbis"));
    }

    #[test]
    fn chunks_join_into_one_file() {
        let a = wav_from_pcm(&[1, 0, 2, 0], 24_000);
        let b = wav_from_pcm(&[3, 0], 24_000);
        let joined = join(vec![a, b]).unwrap();
        let (_, data) = wav_parts(&joined).unwrap();
        assert_eq!(data, [1, 0, 2, 0, 3, 0]);
        let mp3 = join(vec![b"ID3a".to_vec(), b"ID3b".to_vec()]).unwrap();
        assert_eq!(mp3, b"ID3aID3b");
        assert!(join(vec![b"ID3a".to_vec(), b"OggS".to_vec()]).is_err());
    }

    #[test]
    fn durations_come_from_the_wav_header_or_ffmpegs_description() {
        // 16-bit mono at 8 kHz: 16,000 bytes a second.
        let wav = wav_from_pcm(&vec![0; 40_000], 8_000);
        assert_eq!(wav_seconds(&wav), Some(2.5));
        assert_eq!(wav_seconds(b"ID3 not a wav"), None);
        let described =
            "Input #0, ogg, from 'a.ogg':\n  Duration: 01:02:03.50, start: 0.0, bitrate: 32 kb/s";
        assert_eq!(parse_duration(described), Some(3_723.5));
        assert_eq!(parse_duration("no duration here"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_managed_ffmpeg_comes_from_uv_once_and_falls_back_to_the_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ffmpeg-managed");
        std::fs::write(&exe, "").unwrap();
        let uv = dir.path().join("uv");
        let log = dir.path().join("calls");
        std::fs::write(
            &uv,
            format!(
                "#!/bin/sh\necho call >> '{}'\nprintf '%s' '{}'\n",
                log.display(),
                exe.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ffmpeg = Ffmpeg::managed(uv);
        assert_eq!(ffmpeg.exe().await, Some(&exe));
        assert_eq!(ffmpeg.exe().await, Some(&exe));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        let broken = Ffmpeg::managed(dir.path().join("no-uv"));
        assert_eq!(broken.exe().await.cloned(), path_ffmpeg());
    }
}
