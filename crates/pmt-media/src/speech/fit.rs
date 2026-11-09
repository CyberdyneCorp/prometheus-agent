//! Recordings over a cloud transcriber's upload limits (spec: voice-mode,
//! Long recordings and provider upload limits). An over-limit recording
//! is first re-encoded to 16 kHz mono Opus; when that is still over, it
//! is cut at pauses into pieces within the limit, which are transcribed
//! in order and joined. This covers voice-mode recordings, gateway voice
//! notes and audio attachments alike: they all reach
//! [`crate::MediaSession::transcribe`]. Local providers declare no limits
//! and are never split.

use std::path::Path;

use super::audio::{self, wav_from_pcm};
use super::stt::UploadLimit;
use super::tempdir;
use crate::MediaSession;

const MIB: u64 = 1 << 20;

/// Pieces aim this far under the limit (encoders vary per piece).
const MARGIN: f64 = 0.9;

/// One upload: its bytes and file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) bytes: Vec<u8>,
    pub(crate) name: String,
}

/// Why it is over: the size or the length, named for the error.
fn over(limit: UploadLimit, provider: &str, bytes: usize, seconds: Option<f64>) -> Option<String> {
    if bytes as u64 > limit.bytes {
        return Some(format!(
            "the audio is {} MiB, over {provider}'s {} MiB upload limit",
            bytes as u64 / MIB,
            limit.bytes / MIB
        ));
    }
    #[allow(clippy::cast_precision_loss)]
    match (seconds, limit.seconds) {
        (Some(seconds), Some(most)) if seconds > most as f64 => Some(format!(
            "the audio is {seconds:.0} s long, over {provider}'s {most} s upload limit"
        )),
        _ => None,
    }
}

/// Where to cut `samples` (at `rate` Hz) so that no piece is longer than
/// `max` samples: in the quietest 100 ms of the last two fifths of each
/// piece, so words are not cut in half.
#[must_use]
pub fn split_points(samples: &[i16], rate: u32, max: usize) -> Vec<usize> {
    let window = usize::try_from(rate / 10).unwrap_or(1).max(1);
    let step = (window / 2).max(1);
    let max = max.max(window * 2);
    let mut cuts = Vec::new();
    let mut start = 0;
    while samples.len() - start > max {
        let end = start + max;
        let mut at = start + max * 3 / 5;
        let (mut best, mut quietest) = (end, f64::MAX);
        while at + window <= end {
            let level = rms(&samples[at..at + window]);
            if level < quietest {
                quietest = level;
                best = at + window / 2;
            }
            at += step;
        }
        cuts.push(best);
        start = best;
    }
    cuts
}

fn rms(samples: &[i16]) -> f64 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
    #[allow(clippy::cast_precision_loss)]
    (sum / samples.len().max(1) as f64).sqrt()
}

impl MediaSession {
    /// The length of the audio at `path` in seconds, when it can be told.
    async fn seconds(&self, path: &Path, bytes: &[u8]) -> Option<f64> {
        match audio::wav_seconds(bytes) {
            Some(seconds) => Some(seconds),
            None => self.speech.ffmpeg.duration(path).await,
        }
    }

    /// The uploads `provider` takes for the audio at `path`: the file
    /// itself when it is within `limit`, else a re-encoded copy, else
    /// pieces cut at pauses.
    ///
    /// # Errors
    ///
    /// It is over the limit and cannot be re-encoded or split (no
    /// ffmpeg for a compressed file), or ffmpeg failed.
    pub(crate) async fn fit(
        &self,
        limit: UploadLimit,
        provider: &str,
        path: &Path,
        bytes: Vec<u8>,
    ) -> Result<Vec<Piece>, String> {
        let seconds = self.seconds(path, &bytes).await;
        let Some(why) = over(limit, provider, bytes.len(), seconds) else {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("audio.ogg")
                .to_owned();
            return Ok(vec![Piece { bytes, name }]);
        };
        let dir = tempdir(self.setup())?;
        let fitted = self
            .shrink(limit, provider, path, &bytes, seconds, &dir)
            .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        fitted.map_err(|failure| {
            failure.unwrap_or_else(|| format!("{why}; ffmpeg is needed to re-encode or split it"))
        })
    }

    /// Re-encodes, then splits; `Err(None)` when neither is possible.
    async fn shrink(
        &self,
        limit: UploadLimit,
        provider: &str,
        path: &Path,
        bytes: &[u8],
        seconds: Option<f64>,
        dir: &Path,
    ) -> Result<Vec<Piece>, Option<String>> {
        let ffmpeg = &self.speech.ffmpeg;
        let mut encoded_rate = None;
        if ffmpeg.available().await {
            let encoded = dir.join("speech.ogg");
            ffmpeg.encode_speech(path, &encoded).await.map_err(Some)?;
            let compact = tokio::fs::read(&encoded)
                .await
                .map_err(|e| Some(e.to_string()))?;
            if over(limit, provider, compact.len(), seconds).is_none() {
                return Ok(vec![Piece {
                    bytes: compact,
                    name: "speech.ogg".to_owned(),
                }]);
            }
            #[allow(clippy::cast_precision_loss)]
            if let Some(seconds) = seconds.filter(|s| *s > 0.0) {
                encoded_rate = Some(compact.len() as f64 / seconds);
            }
        }
        let (rate, samples) = match audio::wav_samples(bytes) {
            Some(decoded) => decoded,
            None if ffmpeg.available().await => {
                let wav = dir.join("speech.wav");
                ffmpeg.decode_speech(path, &wav).await.map_err(Some)?;
                let decoded = tokio::fs::read(&wav)
                    .await
                    .map_err(|e| Some(e.to_string()))?;
                audio::wav_samples(&decoded).ok_or_else(|| Some("ffmpeg made no WAV".to_owned()))?
            }
            None => return Err(None),
        };
        self.pieces(limit, provider, rate, &samples, encoded_rate, dir)
            .await
    }

    /// Cuts `samples` at pauses into uploads within `limit`: Opus pieces
    /// when ffmpeg is here (`encoded_rate` bytes a second), else WAV.
    async fn pieces(
        &self,
        limit: UploadLimit,
        provider: &str,
        rate: u32,
        samples: &[i16],
        encoded_rate: Option<f64>,
        dir: &Path,
    ) -> Result<Vec<Piece>, Option<String>> {
        #[allow(clippy::cast_precision_loss)]
        let bytes_per_second = encoded_rate.unwrap_or(f64::from(rate) * 2.0);
        #[allow(clippy::cast_precision_loss)]
        let by_size = limit.bytes.saturating_sub(64) as f64 / bytes_per_second;
        #[allow(clippy::cast_precision_loss)]
        let most = limit.seconds.map_or(by_size, |s| by_size.min(s as f64)) * MARGIN;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max = (most * f64::from(rate)) as usize;
        let mut bounds = vec![0];
        bounds.extend(split_points(samples, rate, max));
        bounds.push(samples.len());
        let mut pieces = Vec::with_capacity(bounds.len() - 1);
        for (n, pair) in bounds.windows(2).enumerate() {
            let pcm: Vec<u8> = samples[pair[0]..pair[1]]
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            let wav = wav_from_pcm(&pcm, rate);
            let piece = if encoded_rate.is_some() {
                self.encode_piece(&wav, n, dir).await.map_err(Some)?
            } else {
                Piece {
                    bytes: wav,
                    name: format!("piece-{n}.wav"),
                }
            };
            #[allow(clippy::cast_precision_loss)]
            let seconds = (pair[1] - pair[0]) as f64 / f64::from(rate);
            if let Some(why) = over(limit, provider, piece.bytes.len(), Some(seconds)) {
                return Err(Some(format!("a piece is still too large: {why}")));
            }
            pieces.push(piece);
        }
        Ok(pieces)
    }

    async fn encode_piece(&self, wav: &[u8], n: usize, dir: &Path) -> Result<Piece, String> {
        let raw = dir.join(format!("piece-{n}.wav"));
        let encoded = dir.join(format!("piece-{n}.ogg"));
        tokio::fs::write(&raw, wav)
            .await
            .map_err(|e| e.to_string())?;
        self.speech.ffmpeg.encode_speech(&raw, &encoded).await?;
        let bytes = tokio::fs::read(&encoded).await.map_err(|e| e.to_string())?;
        let _ = tokio::fs::remove_file(&raw).await;
        Ok(Piece {
            bytes,
            name: format!("piece-{n}.ogg"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_land_in_the_pauses() {
        // 10 Hz "speech" (loud) with a quiet second at 7-8 s and 15-16 s.
        let rate = 100;
        let samples: Vec<i16> = (0..2_000)
            .map(|i| match i / 100 {
                7 | 15 => 3,
                _ => 3_000,
            })
            .collect();
        let cuts = split_points(&samples, rate, 900);
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        assert!((700..800).contains(&cuts[0]), "{cuts:?}");
        assert!((1_500..1_600).contains(&cuts[1]), "{cuts:?}");
        assert!(split_points(&samples, rate, 5_000).is_empty());
    }

    #[test]
    fn over_names_size_before_length() {
        let limit = UploadLimit {
            bytes: 25 * MIB,
            seconds: Some(3_600),
        };
        assert_eq!(
            over(limit, "x", 30 << 20, Some(10.0)).unwrap(),
            "the audio is 30 MiB, over x's 25 MiB upload limit"
        );
        assert!(over(limit, "x", 1, Some(3_600.0)).is_none());
        assert!(
            over(limit, "x", 1, Some(3_601.0))
                .unwrap()
                .contains("3601 s long")
        );
    }
}
