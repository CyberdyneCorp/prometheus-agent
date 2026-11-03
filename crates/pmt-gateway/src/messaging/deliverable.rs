//! Deliverable mode (spec: platform-adapters, Deliverable mode): a final
//! reply that names a file by bare absolute or `~/` path, outside code,
//! with a deliverable extension, delivers that file and drops the path
//! from the text. Images go inline, video inline where the platform sends
//! video, audio as a voice note or audio file, and documents, data,
//! archives and web pages as uploads; source code and logs never. Each
//! path passes the attachment path checks ([`Roots::admit`]).

use std::collections::BTreeSet;

use super::adapter::OutboundAttachment;
use super::attachments::Roots;
use super::descriptor::MediaKind;

const IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg"];
const VIDEO: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "3gp"];
const AUDIO: &[&str] = &["mp3", "wav", "ogg", "opus", "m4a", "flac"];
const DOCUMENTS: &[&str] = &[
    "pdf", "docx", "doc", "odt", "rtf", "txt", "md", "epub", "xlsx", "xls", "ods", "csv", "tsv",
    "json", "xml", "yaml", "yml", "kmz", "kml", "geojson", "gpx", "pptx", "ppt", "odp", "key",
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "html", "htm",
];

/// How a file of `path` is delivered on a platform sending `media_out`;
/// `None` for a file that is never auto-delivered.
#[must_use]
pub fn delivery(path: &str, media_out: &BTreeSet<MediaKind>) -> Option<OutboundAttachment> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let ext = ext.as_str();
    let mut file = OutboundAttachment::new(path);
    if IMAGES.contains(&ext) {
        return Some(file);
    }
    if VIDEO.contains(&ext) {
        file.as_document = !media_out.contains(&MediaKind::Video);
        return Some(file);
    }
    if AUDIO.contains(&ext) {
        file.as_voice = matches!(ext, "ogg" | "opus") && media_out.contains(&MediaKind::Voice);
        return Some(file);
    }
    if DOCUMENTS.contains(&ext) {
        file.as_document = true;
        return Some(file);
    }
    None
}

/// Characters around a path that are punctuation, not part of it.
const LEADING: &[char] = &['(', '[', '"', '\'', '<'];
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'', '>'];

/// The text of `body` without the deliverable paths it names, and the
/// files to deliver (admitted by `roots` only).
#[must_use]
pub fn extract(
    body: &str,
    roots: &Roots,
    media_out: &BTreeSet<MediaKind>,
) -> (String, Vec<OutboundAttachment>) {
    let mut files: Vec<OutboundAttachment> = Vec::new();
    let mut fence: Option<&str> = None;
    let lines: Vec<String> = body
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(open) = fence {
                if trimmed.starts_with(open) {
                    fence = None;
                }
                return line.to_owned();
            }
            if let Some(open) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
                fence = Some(open);
                return line.to_owned();
            }
            strip_line(line, roots, media_out, &mut files)
        })
        .collect();
    (lines.join("\n").trim().to_owned(), files)
}

/// `line` without its deliverable paths (outside inline code).
fn strip_line(
    line: &str,
    roots: &Roots,
    media_out: &BTreeSet<MediaKind>,
    files: &mut Vec<OutboundAttachment>,
) -> String {
    let mut out = String::with_capacity(line.len());
    for (i, segment) in line.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            // Inside inline code.
            out.push_str(segment);
            continue;
        }
        let kept: Vec<&str> = segment
            .split(' ')
            .filter(|word| !take(word, roots, media_out, files))
            .collect();
        out.push_str(&kept.join(" "));
    }
    out.trim_end().to_owned()
}

/// Whether `word` is a deliverable path (then attached).
fn take(
    word: &str,
    roots: &Roots,
    media_out: &BTreeSet<MediaKind>,
    files: &mut Vec<OutboundAttachment>,
) -> bool {
    let candidate = word.trim_start_matches(LEADING).trim_end_matches(TRAILING);
    if !(candidate.starts_with('/') || candidate.starts_with("~/")) {
        return false;
    }
    let (Some(mut file), Some(real)) = (delivery(candidate, media_out), roots.admit(candidate))
    else {
        return false;
    };
    file.path = real.to_string_lossy().into_owned();
    if !files.iter().any(|f| f.path == file.path) {
        files.push(file);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> BTreeSet<MediaKind> {
        [
            MediaKind::Image,
            MediaKind::Audio,
            MediaKind::Voice,
            MediaKind::Video,
            MediaKind::Document,
        ]
        .into_iter()
        .collect()
    }

    fn workspace() -> (tempfile::TempDir, Roots) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        for name in ["report.pdf", "chart.png", "main.rs", "clip.mp4", "note.ogg"] {
            std::fs::write(project.join(name), name).unwrap();
        }
        let roots = Roots {
            user_home: Some(home),
            ..Roots::of_profile(&dir.path().join("profile"), Some(&project))
        };
        (dir, roots)
    }

    #[test]
    fn a_report_path_becomes_an_upload() {
        let (_dir, roots) = workspace();
        let (text, files) = extract("Done: ~/project/report.pdf", &roots, &every_kind());
        assert_eq!(text, "Done:");
        assert_eq!(files.len(), 1);
        assert!(files[0].as_document);
        assert!(files[0].path.ends_with("report.pdf"));
    }

    #[test]
    fn code_blocks_and_inline_code_are_untouched() {
        let (_dir, roots) = workspace();
        let body = "Look:\n```\n~/project/chart.png\n```\nand `~/project/report.pdf`";
        let (text, files) = extract(body, &roots, &every_kind());
        assert_eq!(text, body);
        assert!(files.is_empty());
    }

    #[test]
    fn classes_decide_how_files_go_and_code_never_does() {
        let (_dir, roots) = workspace();
        let body = "See ~/project/chart.png, ~/project/clip.mp4 and (~/project/note.ogg). Code: ~/project/main.rs";
        let (text, files) = extract(body, &roots, &every_kind());
        assert_eq!(text, "See and Code: ~/project/main.rs");
        let kinds: Vec<(bool, bool)> = files.iter().map(|f| (f.as_document, f.as_voice)).collect();
        assert_eq!(kinds, [(false, false), (false, false), (false, true)]);
        let no_video: BTreeSet<MediaKind> = [MediaKind::Image, MediaKind::Document].into();
        let (_, files) = extract("~/project/clip.mp4", &roots, &no_video);
        assert!(files[0].as_document);
        let (text, files) = extract("Missing: ~/project/gone.pdf", &roots, &every_kind());
        assert_eq!(text, "Missing: ~/project/gone.pdf");
        assert!(files.is_empty());
    }
}
