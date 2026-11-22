//! iTerm2 inline images (OSC 1337 `File`), sized to the frame's cell box.

use image::RgbaImage;

pub(super) fn encode(frame: &RgbaImage, cols: u16, rows: u16) -> String {
    let payload = super::kitty::png_base64(frame);
    format!(
        "\x1b]1337;File=inline=1;size={};preserveAspectRatio=1;width={cols};height={rows}:{payload}\x07",
        payload.len()
    )
}
