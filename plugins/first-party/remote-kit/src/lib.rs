//! What the first-party remote `terminal-backend` plugins share: the shell
//! script a command line runs as on the remote side (its directory, its
//! environment, its timeout and the marker that reports where it ended)
//! and reading that marker back out of the output.
//!
//! The host's tests (`crates/pmt-plugins/tests/first_party.rs`) run this
//! through the plugins against fake provider APIs.

/// `text` as one POSIX shell word.
#[must_use]
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The line printed after the command, followed by its final directory.
#[must_use]
pub fn marker(id: &str) -> String {
    let id: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    format!("__PMT_CWD_{id}__")
}

/// The script `line` runs as: in `cwd`, with `env` exported, then the
/// marker and the directory it finished in, keeping the command's status.
/// A `timeout_ms` other than `u64::MAX` wraps it in `timeout`, which ends
/// it with status 124.
#[must_use]
pub fn script(
    line: &str,
    cwd: &str,
    env: &[(String, String)],
    marker: &str,
    timeout_ms: u64,
) -> String {
    let mut body = format!("cd {} || exit 126\n", quote(cwd));
    for (name, value) in env {
        if is_name(name) {
            body.push_str(&format!("export {name}={}\n", quote(value)));
        }
    }
    body.push_str(&format!(
        "{{ {line}\n}}\n__pmt_status=$?\nprintf '\\n%s%s\\n' {} \"$(pwd)\"\nexit $__pmt_status\n",
        quote(marker)
    ));
    if timeout_ms == u64::MAX {
        return body;
    }
    let secs = timeout_ms.div_ceil(1000).max(1);
    format!("timeout {secs} sh -c {}", quote(&body))
}

/// A shell variable name (anything else is not exported).
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `output` without the marker line, and the directory the marker named
/// (none when the command ended the shell before it was printed).
#[must_use]
pub fn split(output: &str, marker: &str) -> (String, Option<String>) {
    let Some(at) = output.rfind(marker) else {
        return (output.to_owned(), None);
    };
    let cwd = output[at + marker.len()..]
        .lines()
        .next()
        .unwrap_or("")
        .to_owned();
    let mut before = &output[..at];
    // The newline printed before the marker is ours, not the command's.
    before = before.strip_suffix('\n').unwrap_or(before);
    (before.to_owned(), Some(cwd).filter(|c| !c.is_empty()))
}

/// `text` for a URL query value or path segment.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A `multipart/form-data` body with one file field: the content type
/// (naming the boundary) and the body.
#[must_use]
pub fn multipart(field: &str, filename: &str, contents: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----prometheus-upload-7c3f9a";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n",
        filename.replace('"', "")
    )
    .into_bytes();
    body.extend_from_slice(contents);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The last path segment of `path`.
#[must_use]
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A gzip-compressed tar archive holding one regular file at `path`
/// (absolute paths are stored relative to `/`). The deflate stream uses
/// stored blocks: no compression, nothing to depend on.
#[must_use]
pub fn tar_gz(path: &str, contents: &[u8]) -> Vec<u8> {
    gzip(&tar(path.trim_start_matches('/'), contents))
}

/// A ustar archive of one file.
fn tar(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut header = [0_u8; 512];
    let (prefix, name) = if name.len() > 100 {
        // The ustar prefix field holds the directories of a long path.
        let cut = name[..name.len().min(156)].rfind('/').unwrap_or(0);
        (&name[..cut], &name[cut + 1..])
    } else {
        ("", name)
    };
    put(&mut header[0..100], name.as_bytes());
    put(&mut header[100..108], b"0000644\0");
    put(&mut header[108..116], b"0000000\0");
    put(&mut header[116..124], b"0000000\0");
    put(
        &mut header[124..136],
        format!("{:011o}\0", contents.len()).as_bytes(),
    );
    put(&mut header[136..148], b"00000000000\0");
    header[148..156].fill(b' ');
    header[156] = b'0';
    put(&mut header[257..263], b"ustar\0");
    put(&mut header[263..265], b"00");
    put(&mut header[345..500], prefix.as_bytes());
    let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    put(&mut header[148..156], format!("{sum:06o}\0 ").as_bytes());
    let mut out = header.to_vec();
    out.extend_from_slice(contents);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out.extend_from_slice(&[0_u8; 1024]);
    out
}

fn put(field: &mut [u8], value: &[u8]) {
    let n = value.len().min(field.len());
    field[..n].copy_from_slice(&value[..n]);
}

/// `data` as a gzip member of stored deflate blocks.
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut blocks = data.chunks(0xffff).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(u32::try_from(data.len() & 0xffff_ffff).unwrap_or(0)).to_le_bytes());
    out
}

/// The CRC-32 (IEEE) gzip ends with.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
