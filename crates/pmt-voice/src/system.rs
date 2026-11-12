//! This machine's audio through the command-line tools it has: capture
//! with ffmpeg (`AVFoundation` on macOS, `DirectShow` on Windows) or the
//! `PulseAudio`/`PipeWire` and ALSA recorders on Linux, playback with ffplay,
//! afplay, paplay, pw-play, aplay or PowerShell. No audio library is
//! linked, so every target (musl and Android included) builds the same;
//! a host without these tools simply has no input device.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};

use crate::device::{BoxFuture, Capture, Clip, Microphone, Player, Sound};
use crate::pcm;

/// Bytes read from a recorder at a time (100 ms).
const READ_BYTES: usize = 3_200;

/// Where the reference compose file's `audio` profile mounts the host's
/// PulseAudio-compatible socket inside the container (spec:
/// distribution-and-update, Container audio bridge and host services).
pub const CONTAINER_PULSE_SOCKET: &str = "/run/pulse/native";

/// The `PULSE_SERVER` the audio tools need: the mounted container socket
/// when it exists and nothing else names a server, so device probing
/// finds the host's devices instead of reporting none.
fn pulse_server(socket: &Path, configured: Option<&std::ffi::OsStr>) -> Option<String> {
    (configured.is_none_or(std::ffi::OsStr::is_empty) && socket.exists())
        .then(|| format!("unix:{}", socket.display()))
}

/// Points an audio tool at the container's `PulseAudio` socket when needed.
fn with_pulse(command: &mut Command) {
    let configured = std::env::var_os("PULSE_SERVER");
    if let Some(server) = pulse_server(Path::new(CONTAINER_PULSE_SOCKET), configured.as_deref()) {
        command.env("PULSE_SERVER", server);
    }
}

/// The executable `name` on `PATH`, if any.
#[must_use]
pub fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<OsString> = if cfg!(windows) {
        vec![".exe".into(), ".cmd".into(), ".bat".into()]
    } else {
        vec![OsString::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let mut file = OsString::from(name);
            file.push(ext);
            let candidate = dir.join(file);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// How audio is recorded here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recorder {
    /// ffmpeg's `AVFoundation` input (macOS).
    Avfoundation(PathBuf),
    /// ffmpeg's `DirectShow` input (Windows).
    Dshow(PathBuf),
    /// `parecord`, devices from `pactl` (`PulseAudio` or `PipeWire`).
    Pulse { parecord: PathBuf, pactl: PathBuf },
    /// `arecord`, devices from `arecord -l` (ALSA).
    Alsa(PathBuf),
}

/// The microphone of this machine.
#[derive(Debug, Clone)]
pub struct SystemMicrophone {
    recorder: Recorder,
}

impl SystemMicrophone {
    /// The recorder this host has (`ffmpeg` is the one to use for the
    /// ffmpeg inputs); `None` when there is none.
    #[must_use]
    pub fn detect(ffmpeg: Option<PathBuf>) -> Option<Self> {
        let recorder = if cfg!(target_os = "macos") {
            Recorder::Avfoundation(ffmpeg?)
        } else if cfg!(windows) {
            Recorder::Dshow(ffmpeg?)
        } else if let (Some(parecord), Some(pactl)) = (find("parecord"), find("pactl")) {
            Recorder::Pulse { parecord, pactl }
        } else {
            Recorder::Alsa(find("arecord")?)
        };
        Some(Self { recorder })
    }

    fn command(&self, devices: &[String]) -> Command {
        let device = devices.first().cloned().unwrap_or_default();
        let mut command = match &self.recorder {
            Recorder::Avfoundation(ffmpeg) | Recorder::Dshow(ffmpeg) => {
                let input = if matches!(self.recorder, Recorder::Dshow(_)) {
                    [
                        "-f".into(),
                        "dshow".into(),
                        "-i".into(),
                        format!("audio={device}"),
                    ]
                } else {
                    ["-f".into(), "avfoundation".into(), "-i".into(), ":0".into()]
                };
                let mut command = Command::new(ffmpeg);
                command
                    .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
                    .args(input)
                    .args(["-ac", "1", "-ar", "16000", "-f", "s16le", "-"]);
                command
            }
            Recorder::Pulse { parecord, .. } => {
                let mut command = Command::new(parecord);
                command.args(["--raw", "--format=s16le", "--rate=16000", "--channels=1"]);
                if !device.is_empty() {
                    command.arg(format!("--device={device}"));
                }
                command
            }
            Recorder::Alsa(arecord) => {
                let mut command = Command::new(arecord);
                command.args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-t", "raw"]);
                command
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        with_pulse(&mut command);
        command
    }
}

/// The text a lister prints (stdout and stderr: ffmpeg lists on stderr).
async fn listing(program: &Path, args: &[&str]) -> String {
    let mut command = Command::new(program);
    with_pulse(&mut command);
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(10), output).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text
        }
        _ => String::new(),
    }
}

impl Microphone for SystemMicrophone {
    fn devices(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            match &self.recorder {
                Recorder::Avfoundation(ffmpeg) => avfoundation_devices(
                    &listing(
                        ffmpeg,
                        &[
                            "-hide_banner",
                            "-f",
                            "avfoundation",
                            "-list_devices",
                            "true",
                            "-i",
                            "",
                        ],
                    )
                    .await,
                ),
                Recorder::Dshow(ffmpeg) => dshow_devices(
                    &listing(
                        ffmpeg,
                        &[
                            "-hide_banner",
                            "-list_devices",
                            "true",
                            "-f",
                            "dshow",
                            "-i",
                            "dummy",
                        ],
                    )
                    .await,
                ),
                Recorder::Pulse { pactl, .. } => {
                    pulse_sources(&listing(pactl, &["list", "short", "sources"]).await)
                }
                Recorder::Alsa(arecord) => alsa_cards(&listing(arecord, &["-l"]).await),
            }
        })
    }

    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Capture>, String>> {
        Box::pin(async move {
            let devices = self.devices().await;
            if devices.is_empty() {
                return Err(crate::mode::NO_DEVICE.to_owned());
            }
            let child = self
                .command(&devices)
                .spawn()
                .map_err(|e| format!("the recorder could not start: {e}"))?;
            Ok(Box::new(Recording { child }) as Box<dyn Capture>)
        })
    }
}

struct Recording {
    child: Child,
}

impl Capture for Recording {
    fn read(&mut self) -> BoxFuture<'_, Option<Vec<i16>>> {
        Box::pin(async move {
            let stdout = self.child.stdout.as_mut()?;
            let mut buffer = vec![0u8; READ_BYTES];
            let mut filled = 0;
            while filled < READ_BYTES {
                match stdout.read(&mut buffer[filled..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => filled += n,
                }
            }
            (filled >= 2).then(|| pcm::samples_of(&buffer[..filled]))
        })
    }
}

/// `[AVFoundation indev @ 0x..] [0] MacBook Pro Microphone` lines after
/// `AVFoundation audio devices:`.
fn avfoundation_devices(listing: &str) -> Vec<String> {
    listing
        .lines()
        .skip_while(|line| !line.contains("AVFoundation audio devices"))
        .skip(1)
        .filter_map(|line| {
            let rest = line.split_once("] [")?.1;
            let (_, name) = rest.split_once("] ")?;
            Some(name.trim().to_owned())
        })
        .collect()
}

/// `[dshow @ ..] "Microphone (Realtek Audio)" (audio)` lines.
fn dshow_devices(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter(|line| line.trim_end().ends_with("(audio)"))
        .filter_map(|line| {
            let start = line.find('"')? + 1;
            let end = start + line[start..].find('"')?;
            Some(line[start..end].to_owned())
        })
        .collect()
}

/// Source names of `pactl list short sources`, without the monitors of
/// outputs.
fn pulse_sources(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .filter(|name| !name.ends_with(".monitor"))
        .map(str::to_owned)
        .collect()
}

/// `card 0: PCH [HDA Intel PCH], device 0: ALC3246 Analog [ALC3246
/// Analog]` lines of `arecord -l`.
fn alsa_cards(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter(|line| line.starts_with("card "))
        .filter_map(|line| {
            let card = line.split_once('[')?.1.split_once(']')?.0;
            let device = line.rsplit_once('[')?.1.trim_end_matches(']');
            Some(format!("{card}: {device}"))
        })
        .collect()
}

/// How audio is played here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Output {
    /// `ffplay`: any format, raw PCM on stdin.
    Ffplay(PathBuf),
    /// `afplay` (macOS): WAV, MP3, AAC, AIFF, FLAC files.
    Afplay(PathBuf),
    /// `paplay`: WAV, Ogg and FLAC files, raw PCM on stdin.
    Paplay(PathBuf),
    /// `pw-play`: WAV, Ogg and FLAC files, raw PCM on stdin.
    PwPlay(PathBuf),
    /// `aplay`: WAV files, raw PCM on stdin.
    Aplay(PathBuf),
    /// Windows `SoundPlayer` through PowerShell: WAV files.
    PowerShell(PathBuf),
}

/// The speakers of this machine.
#[derive(Debug, Clone)]
pub struct SystemPlayer {
    output: Output,
    ffmpeg: Option<PathBuf>,
    scratch: PathBuf,
}

/// The format of an audio file by its first bytes.
fn format_of(head: &[u8]) -> &'static str {
    match head {
        [b'R', b'I', b'F', b'F', ..] => "wav",
        [b'O', b'g', b'g', b'S', ..] => "ogg",
        [b'f', b'L', b'a', b'C', ..] => "flac",
        [b'I', b'D', b'3', ..] | [0xFF, 0xE0..=0xFF, ..] => "mp3",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "m4a",
        _ => "audio",
    }
}

impl SystemPlayer {
    /// The player this host has; temporary WAV files go to `scratch`.
    /// `ffmpeg` converts formats the player cannot read.
    #[must_use]
    pub fn detect(ffmpeg: Option<PathBuf>, scratch: &Path) -> Option<Self> {
        let output = if let Some(ffplay) = find("ffplay") {
            Output::Ffplay(ffplay)
        } else if cfg!(target_os = "macos") {
            Output::Afplay(find("afplay")?)
        } else if cfg!(windows) {
            Output::PowerShell(find("powershell").or_else(|| find("pwsh"))?)
        } else if let Some(paplay) = find("paplay") {
            Output::Paplay(paplay)
        } else if let Some(pw_play) = find("pw-play") {
            Output::PwPlay(pw_play)
        } else {
            Output::Aplay(find("aplay")?)
        };
        Some(Self {
            output,
            ffmpeg,
            scratch: scratch.to_path_buf(),
        })
    }

    fn reads(&self, format: &str) -> bool {
        match &self.output {
            Output::Ffplay(_) => true,
            Output::Afplay(_) => matches!(format, "wav" | "mp3" | "m4a" | "flac"),
            Output::Paplay(_) | Output::PwPlay(_) => matches!(format, "wav" | "ogg" | "flac"),
            Output::Aplay(_) | Output::PowerShell(_) => format == "wav",
        }
    }

    fn file_command(&self, file: &Path) -> Command {
        let mut command = match &self.output {
            Output::Ffplay(exe) => {
                let mut c = Command::new(exe);
                c.args(["-nodisp", "-autoexit", "-loglevel", "error"])
                    .arg(file);
                c
            }
            Output::Afplay(exe) | Output::Paplay(exe) | Output::PwPlay(exe) => {
                let mut c = Command::new(exe);
                c.arg(file);
                c
            }
            Output::Aplay(exe) => {
                let mut c = Command::new(exe);
                c.arg("-q").arg(file);
                c
            }
            Output::PowerShell(exe) => {
                let quoted = file.to_string_lossy().replace('\'', "''");
                let mut c = Command::new(exe);
                c.args(["-NoProfile", "-NonInteractive", "-Command"])
                    .arg(format!(
                        "(New-Object Media.SoundPlayer '{quoted}').PlaySync()"
                    ));
                c
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        with_pulse(&mut command);
        command
    }

    /// The command reading raw mono s16le PCM at `rate` from stdin, when
    /// the player can.
    fn stream_command(&self, rate: u32) -> Option<Command> {
        let mut command = match &self.output {
            Output::Ffplay(exe) => {
                let mut c = Command::new(exe);
                c.args(["-nodisp", "-autoexit", "-loglevel", "error", "-f", "s16le"])
                    .args(["-ar", &rate.to_string(), "-ch_layout", "mono", "-i", "-"]);
                c
            }
            Output::Paplay(exe) => {
                let mut c = Command::new(exe);
                c.args(["--raw", "--format=s16le", "--channels=1"])
                    .arg(format!("--rate={rate}"));
                c
            }
            Output::PwPlay(exe) => {
                let mut c = Command::new(exe);
                c.args([
                    "--format",
                    "s16",
                    "--channels",
                    "1",
                    "--rate",
                    &rate.to_string(),
                    "-",
                ]);
                c
            }
            Output::Aplay(exe) => {
                let mut c = Command::new(exe);
                c.args([
                    "-q",
                    "-f",
                    "S16_LE",
                    "-c",
                    "1",
                    "-t",
                    "raw",
                    "-r",
                    &rate.to_string(),
                ]);
                c
            }
            Output::Afplay(_) | Output::PowerShell(_) => return None,
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        with_pulse(&mut command);
        Some(command)
    }

    async fn play_file(&self, file: &Path) -> Result<(), String> {
        let mut head = [0u8; 12];
        let read = tokio::fs::File::open(file)
            .await
            .map_err(|e| format!("{}: {e}", file.display()))?
            .read(&mut head)
            .await
            .unwrap_or(0);
        let format = format_of(&head[..read]);
        if self.reads(format) {
            return run(self.file_command(file)).await;
        }
        let ffmpeg = self
            .ffmpeg
            .as_ref()
            .ok_or_else(|| format!("cannot play {format} audio here: install ffmpeg"))?;
        let wav = self.scratch_file();
        let converted = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
            .arg(file)
            .args(["-ac", "1", "-ar", "24000"])
            .arg(&wav)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|e| format!("ffmpeg could not start: {e}"))?;
        let played = if converted.success() {
            run(self.file_command(&wav)).await
        } else {
            Err(format!(
                "cannot play {format} audio: ffmpeg could not convert it"
            ))
        };
        let _ = tokio::fs::remove_file(&wav).await;
        played
    }

    async fn play_pcm(&self, rate: u32, samples: &[i16]) -> Result<(), String> {
        let wav = self.scratch_file();
        write_private(&wav, &pcm::wav(samples, rate)).await?;
        let played = run(self.file_command(&wav)).await;
        let _ = tokio::fs::remove_file(&wav).await;
        played
    }

    async fn play_stream(
        &self,
        rate: u32,
        mut chunks: tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Result<(), String> {
        let Some(mut command) = self.stream_command(rate) else {
            let mut bytes = Vec::new();
            while let Some(chunk) = chunks.recv().await {
                bytes.extend(chunk);
            }
            return self.play_pcm(rate, &pcm::samples_of(&bytes)).await;
        };
        let mut child = command
            .spawn()
            .map_err(|e| format!("the player could not start: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            while let Some(chunk) = chunks.recv().await {
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        }
        child
            .wait()
            .await
            .map(drop)
            .map_err(|e| format!("the player failed: {e}"))
    }

    fn scratch_file(&self) -> PathBuf {
        let unique = format!(
            "play-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        self.scratch.join(unique)
    }
}

async fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))
}

async fn run(mut command: Command) -> Result<(), String> {
    let status = command
        .status()
        .await
        .map_err(|e| format!("the player could not start: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the player exited with {status}"))
    }
}

impl Player for SystemPlayer {
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match clip.sound {
                Sound::File(path) => self.play_file(&path).await,
                Sound::Scratch(path) => {
                    let played = self.play_file(&path).await;
                    let _ = tokio::fs::remove_file(&path).await;
                    played
                }
                Sound::Pcm { rate, samples } => self.play_pcm(rate, &samples).await,
                Sound::Stream { rate, chunks } => self.play_stream(rate, chunks).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_mounted_container_socket_becomes_the_pulse_server() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("native");
        assert_eq!(
            super::pulse_server(&socket, None),
            None,
            "no socket mounted"
        );
        std::fs::write(&socket, "").unwrap();
        assert_eq!(
            super::pulse_server(&socket, None),
            Some(format!("unix:{}", socket.display()))
        );
        let configured = std::ffi::OsString::from("tcp:host:4713");
        assert_eq!(super::pulse_server(&socket, Some(&configured)), None);
    }

    use super::*;

    #[test]
    fn avfoundation_lists_only_audio_devices() {
        let listing = "\
[AVFoundation indev @ 0x1] AVFoundation video devices:
[AVFoundation indev @ 0x1] [0] FaceTime HD Camera
[AVFoundation indev @ 0x1] AVFoundation audio devices:
[AVFoundation indev @ 0x1] [0] MacBook Pro Microphone
[AVFoundation indev @ 0x1] [1] BlackHole 2ch
: Input/output error";
        assert_eq!(
            avfoundation_devices(listing),
            ["MacBook Pro Microphone", "BlackHole 2ch"]
        );
        assert!(
            avfoundation_devices("[AVFoundation indev @ 0x1] AVFoundation audio devices:\n")
                .is_empty()
        );
    }

    #[test]
    fn dshow_lists_audio_devices_by_name() {
        let listing = "\
[dshow @ 000001] \"Integrated Camera\" (video)
[dshow @ 000001] \"Microphone Array (Realtek(R) Audio)\" (audio)
[dshow @ 000001]   Alternative name \"@device_cm_{33D9}\"";
        assert_eq!(
            dshow_devices(listing),
            ["Microphone Array (Realtek(R) Audio)"]
        );
    }

    #[test]
    fn pulse_skips_monitors_and_alsa_reads_card_lines() {
        let pulse = "0\talsa_output.pci.analog-stereo.monitor\tmodule-alsa-card.c\ts16le 2ch 48000Hz\tIDLE\n\
1\talsa_input.pci.analog-stereo\tmodule-alsa-card.c\ts16le 2ch 48000Hz\tSUSPENDED\n";
        assert_eq!(pulse_sources(pulse), ["alsa_input.pci.analog-stereo"]);
        let alsa = "**** List of CAPTURE Hardware Devices ****\n\
card 0: PCH [HDA Intel PCH], device 0: ALC3246 Analog [ALC3246 Analog]\n  Subdevices: 1/1\n";
        assert_eq!(alsa_cards(alsa), ["HDA Intel PCH: ALC3246 Analog"]);
        assert!(alsa_cards("").is_empty());
    }

    #[test]
    fn formats_are_told_by_their_first_bytes() {
        assert_eq!(format_of(b"RIFF....WAVE"), "wav");
        assert_eq!(format_of(b"OggS"), "ogg");
        assert_eq!(format_of(b"ID3\x04"), "mp3");
        assert_eq!(format_of(&[0xFF, 0xF3, 0x44]), "mp3");
        assert_eq!(format_of(b"\0\0\0\x20ftypM4A "), "m4a");
    }
}
