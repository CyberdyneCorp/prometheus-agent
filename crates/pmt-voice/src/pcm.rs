//! 16-bit mono PCM: the one format the pipeline works in. Capture
//! delivers it at [`RATE`], silence detection measures it in [`FRAME_MS`]
//! frames, and recordings are written as WAV files for the transcriber.

/// Samples per second of captured audio (what speech-to-text wants).
pub const RATE: u32 = 16_000;

/// Milliseconds of audio in one detection frame.
pub const FRAME_MS: u32 = 20;

/// Samples in one frame at [`RATE`].
pub const FRAME_SAMPLES: usize = (RATE / 1_000 * FRAME_MS) as usize;

/// The root-mean-square level of `samples` on the 0-32767 scale.
#[must_use]
pub fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
    #[allow(clippy::cast_precision_loss)]
    (sum / samples.len() as f64).sqrt()
}

/// Little-endian bytes to samples (a trailing odd byte is dropped).
#[must_use]
pub fn samples_of(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Samples to little-endian bytes.
#[must_use]
pub fn bytes_of(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// `samples` (mono, `rate` Hz) as a WAV file.
#[must_use]
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(samples.len() * 2 + 44);
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
    out.extend_from_slice(&bytes_of(samples));
    out
}

/// A WAV file's sample rate and samples, mixed down to mono; `None` for
/// anything but 16-bit PCM WAV.
#[must_use]
pub fn read_wav(bytes: &[u8]) -> Option<(u32, Vec<i16>)> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut format, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = bytes.get(at + 8..(at + 8 + size).min(bytes.len()))?;
        match &bytes[at..at + 4] {
            b"fmt " => format = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    let format = format?;
    let tag = u16::from_le_bytes(format.get(0..2)?.try_into().ok()?);
    let channels = usize::from(u16::from_le_bytes(format.get(2..4)?.try_into().ok()?));
    let rate = u32::from_le_bytes(format.get(4..8)?.try_into().ok()?);
    let bits = u16::from_le_bytes(format.get(14..16)?.try_into().ok()?);
    if tag != 1 || bits != 16 || channels == 0 {
        return None;
    }
    let interleaved = samples_of(data?);
    let mono = interleaved
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let mean = (sum / channels as i32) as i16;
            mean
        })
        .collect();
    Some((rate, mono))
}

/// A sine tone: `count` beeps of `ms` milliseconds at `hz`, 80 ms apart,
/// at [`RATE`]: the recording start (880 Hz) and end (660 Hz, twice)
/// cues.
#[must_use]
pub fn beep(hz: f64, ms: u32, count: u32) -> Vec<i16> {
    let tone = (RATE / 1_000 * ms) as usize;
    let gap = (RATE / 1_000 * 80) as usize;
    let mut out = Vec::with_capacity((tone + gap) * count as usize);
    for n in 0..count {
        if n > 0 {
            out.resize(out.len() + gap, 0);
        }
        for i in 0..tone {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f64 / f64::from(RATE);
            // A short fade at both ends keeps the beep from clicking.
            #[allow(clippy::cast_precision_loss)]
            let edge = (i.min(tone - i) as f64 / 160.0).min(1.0);
            #[allow(clippy::cast_possible_truncation)]
            let sample = ((2.0 * std::f64::consts::PI * hz * t).sin() * 8_000.0 * edge) as i16;
            out.push(sample);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wav_round_trips_and_stereo_is_mixed_down() {
        let samples = [0, 100, -100, i16::MAX];
        assert_eq!(
            read_wav(&wav(&samples, 8_000)),
            Some((8_000, samples.to_vec()))
        );
        let mut stereo = wav(&[10, 30, -10, -30], 8_000);
        stereo[22] = 2; // channels
        assert_eq!(read_wav(&stereo), Some((8_000, vec![20, -20])));
        assert_eq!(read_wav(b"OggS...."), None);
    }

    #[test]
    fn rms_is_the_level_of_a_constant_signal() {
        assert!((rms(&[300, -300, 300]) - 300.0).abs() < 1e-9);
        assert!(rms(&[]).abs() < f64::EPSILON);
    }

    #[test]
    fn the_end_cue_is_two_beeps() {
        let two = beep(660.0, 120, 2);
        assert_eq!(two.len(), (1_920 + 1_280 + 1_920));
        assert!(rms(&two[..1_920]) > 1_000.0);
        assert!(rms(&two[1_920..3_200]) < 1.0);
    }
}
