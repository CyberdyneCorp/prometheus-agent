//! DEC sixel, with a fixed 6x6x6 colour cube (216 registers) and
//! transparent pixels left unpainted (`P2 = 1`).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use image::RgbaImage;

const OPAQUE: u8 = 32;

fn level(c: u8) -> u16 {
    (u16::from(c) * 5 + 127) / 255
}

/// The cube register of an opaque pixel.
fn register(p: image::Rgba<u8>) -> Option<u16> {
    (p.0[3] > OPAQUE).then(|| level(p.0[0]) * 36 + level(p.0[1]) * 6 + level(p.0[2]))
}

/// One band's line for `colour`, run-length encoded (`!<n><char>`).
fn band_line(frame: &RgbaImage, top: u32, colour: u16) -> String {
    let (width, height) = frame.dimensions();
    let mut line = String::new();
    let mut run: Option<(char, u32)> = None;
    let flush = |line: &mut String, (ch, n): (char, u32)| {
        if n > 3 {
            let _ = write!(line, "!{n}{ch}");
        } else {
            (0..n).for_each(|_| line.push(ch));
        }
    };
    for x in 0..width {
        let mut bits = 0_u8;
        for dy in 0..6 {
            let y = top + dy;
            if y < height && register(*frame.get_pixel(x, y)) == Some(colour) {
                bits |= 1 << dy;
            }
        }
        let ch = char::from(63 + bits);
        run = match run {
            Some((c, n)) if c == ch => Some((c, n + 1)),
            Some(previous) => {
                flush(&mut line, previous);
                Some((ch, 1))
            }
            None => Some((ch, 1)),
        };
    }
    if let Some(last) = run {
        flush(&mut line, last);
    }
    line
}

pub(super) fn encode(frame: &RgbaImage) -> String {
    let (width, height) = frame.dimensions();
    let mut out = format!("\x1bP0;1;0q\"1;1;{width};{height}");
    let used: BTreeSet<u16> = frame.pixels().filter_map(|p| register(*p)).collect();
    for &reg in &used {
        let pct = |n: u16| n * 100 / 5;
        let _ = write!(
            out,
            "#{reg};2;{};{};{}",
            pct(reg / 36),
            pct(reg / 6 % 6),
            pct(reg % 6)
        );
    }
    for top in (0..height).step_by(6) {
        for &reg in &used {
            let _ = write!(out, "#{reg}{}$", band_line(frame, top, reg));
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_colour_frame_defines_two_registers_and_skips_transparency() {
        let frame = RgbaImage::from_fn(12, 12, |x, _| match x {
            0..4 => image::Rgba([255, 0, 0, 255]),
            4..8 => image::Rgba([0, 0, 255, 255]),
            _ => image::Rgba([0, 0, 0, 0]),
        });
        let out = encode(&frame);
        assert!(out.starts_with("\x1bP0;1;0q\"1;1;12;12"));
        assert_eq!(out.matches(";2;").count(), 2);
        assert!(out.contains("!4~"), "{out}");
        assert!(out.ends_with("-\x1b\\"));
    }
}
