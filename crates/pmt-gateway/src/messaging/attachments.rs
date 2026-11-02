//! Outbound attachments (spec: platform-adapters, Outbound attachments
//! and path safety): the agent attaches files with `MEDIA:<path>` lines
//! (deduplicated; ignored inside code blocks, inline code, blockquotes and
//! quoted JSON; `[[as_voice]]` and `[[as_document]]` modifiers), and the
//! gateway delivers a local file only when its fully resolved real path
//! lies inside the session's workspace, the profile's `cache/media/` or
//! its blob store, and is not one of the profile's secret-bearing files.

use std::path::{Path, PathBuf};

use pmt_store::gateway::outbox::OutboundAttachment;

/// The text of a reply and the files its `MEDIA:` lines attach.
#[must_use]
pub fn directives(body: &str) -> (String, Vec<OutboundAttachment>) {
    let mut parser = Parser::default();
    for line in body.lines() {
        parser.line(line);
    }
    let Parser {
        text,
        mut attachments,
        as_document,
        as_voice,
        ..
    } = parser;
    for attachment in &mut attachments {
        attachment.as_document = as_document;
        attachment.as_voice = as_voice && !as_document;
    }
    (text.join("\n").trim().to_owned(), attachments)
}

/// What the lines read so far hold.
#[derive(Default)]
struct Parser<'a> {
    text: Vec<&'a str>,
    attachments: Vec<OutboundAttachment>,
    as_document: bool,
    as_voice: bool,
    /// The fence of the code block the lines are in.
    fence: Option<String>,
}

impl<'a> Parser<'a> {
    fn line(&mut self, line: &'a str) {
        let trimmed = line.trim();
        if let Some(open) = &self.fence {
            if trimmed.starts_with(open.as_str()) {
                self.fence = None;
            }
            self.text.push(line);
            return;
        }
        if let Some(marker) = fence_marker(trimmed) {
            self.fence = Some(marker);
            self.text.push(line);
            return;
        }
        let (rest, document, voice) = modifiers(trimmed);
        self.as_document |= document;
        self.as_voice |= voice;
        match rest.strip_prefix("MEDIA:").map(str::trim) {
            Some(path) => {
                if !path.is_empty() && !self.attachments.iter().any(|a| a.path == path) {
                    self.attachments.push(OutboundAttachment::new(path));
                }
            }
            None if rest.is_empty() && (document || voice) => {}
            None => self.text.push(line),
        }
    }
}

/// The opening fence of a code block (its backticks or tildes).
fn fence_marker(line: &str) -> Option<String> {
    ["```", "~~~"]
        .into_iter()
        .find(|m| line.starts_with(m))
        .map(|m| {
            let run = line.chars().take_while(|c| *c == m.as_bytes()[0] as char);
            run.collect()
        })
}

/// A line without its `[[as_document]]` / `[[as_voice]]` modifiers, and
/// which it carried.
fn modifiers(line: &str) -> (&str, bool, bool) {
    let mut rest = line;
    let (mut document, mut voice) = (false, false);
    loop {
        let trimmed = rest.trim_end();
        if let Some(head) = trimmed.strip_suffix("[[as_document]]") {
            document = true;
            rest = head;
        } else if let Some(head) = trimmed.strip_suffix("[[as_voice]]") {
            voice = true;
            rest = head;
        } else {
            return (trimmed.trim_start(), document, voice);
        }
    }
}

/// Where an agent's attachments may come from.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// The session's workspace, the profile's `cache/media/` and blob
    /// store.
    pub allowed: Vec<PathBuf>,
    /// Files never delivered (the secrets store, `config.toml`,
    /// `state.db`), even inside an allowed root.
    pub refused: Vec<PathBuf>,
    /// The user's home (for `~/`).
    pub user_home: Option<PathBuf>,
}

impl Roots {
    /// The roots of a profile in `profile_dir` and a session working in
    /// `workspace`.
    #[must_use]
    pub fn of_profile(profile_dir: &Path, workspace: Option<&Path>) -> Self {
        let mut allowed = vec![
            profile_dir.join("cache").join("media"),
            profile_dir.join("blobs"),
        ];
        allowed.extend(workspace.map(Path::to_path_buf));
        let refused = ["secrets.toml", "config.toml", "state.db", "grants.toml"]
            .iter()
            .map(|name| profile_dir.join(name))
            .collect();
        Self {
            allowed,
            refused,
            user_home: std::env::home_dir(),
        }
    }

    /// The real path of `path` when it may be delivered.
    #[must_use]
    pub fn admit(&self, path: &str) -> Option<PathBuf> {
        let expanded = match (path.strip_prefix("~/"), &self.user_home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(path),
        };
        let real = std::fs::canonicalize(&expanded).ok()?;
        let canonical = |p: &PathBuf| std::fs::canonicalize(p).ok();
        let refused =
            self.refused.iter().filter_map(canonical).any(|r| {
                real == r
                    || real.file_name().zip(r.file_name()).is_some_and(|(a, b)| {
                        a.to_string_lossy().starts_with(&*b.to_string_lossy())
                    }) && real.parent() == r.parent()
            });
        let inside = self
            .allowed
            .iter()
            .filter_map(canonical)
            .any(|root| real.starts_with(root));
        (inside && !refused && real.is_file()).then_some(real)
    }

    /// Keeps the attachments that may be delivered (with their real
    /// paths) and returns the withheld notes for the others.
    #[must_use]
    pub fn filter(
        &self,
        attachments: Vec<OutboundAttachment>,
    ) -> (Vec<OutboundAttachment>, Vec<String>) {
        let mut kept = Vec::new();
        let mut notes = Vec::new();
        for mut attachment in attachments {
            if let Some(real) = self.admit(&attachment.path) {
                attachment.path = real.to_string_lossy().into_owned();
                kept.push(attachment);
            } else {
                let name = Path::new(&attachment.path).file_name().map_or_else(
                    || attachment.path.clone(),
                    |n| n.to_string_lossy().into_owned(),
                );
                tracing::warn!(
                    "attachment withheld: {} is outside the allowed roots",
                    attachment.path
                );
                notes.push(format!("attachment {name} withheld: path not allowed"));
            }
        }
        (kept, notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_lines_outside_code_and_quotes_attach_files_once() {
        let body = "Here\nMEDIA:/w/a.png\nMEDIA:/w/a.png\n```\nMEDIA:/w/code.png\n```\n\
                    `MEDIA:/w/inline.png`\n> MEDIA:/w/quoted.png\n\"MEDIA:/w/json.png\"\nMEDIA:/w/b.ogg [[as_voice]]";
        let (text, attachments) = directives(body);
        let paths: Vec<&str> = attachments.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths, ["/w/a.png", "/w/b.ogg"]);
        assert!(attachments.iter().all(|a| a.as_voice && !a.as_document));
        assert!(text.contains("MEDIA:/w/code.png") && text.contains("> MEDIA:/w/quoted.png"));
        assert!(!text.contains("MEDIA:/w/a.png"));
        let (_, attachments) = directives("MEDIA:/w/r.pdf\n[[as_document]]");
        assert!(attachments[0].as_document);
    }

    #[test]
    fn secrets_and_symlink_escapes_are_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("default");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(profile.join("cache").join("media")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(profile.join("secrets.toml"), "X = 1").unwrap();
        std::fs::write(workspace.join("chart.png"), "png").unwrap();
        let outside = dir.path().join("passwd");
        std::fs::write(&outside, "root").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("out.png")).unwrap();
        let roots = Roots {
            user_home: Some(dir.path().to_path_buf()),
            ..Roots::of_profile(&profile, Some(&workspace))
        };
        let secrets = OutboundAttachment::new("~/profiles/default/secrets.toml");
        let chart = OutboundAttachment::new(&workspace.join("chart.png").to_string_lossy());
        let (kept, notes) = roots.filter(vec![secrets, chart]);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            notes,
            ["attachment secrets.toml withheld: path not allowed"]
        );
        #[cfg(unix)]
        assert!(
            roots
                .admit(&workspace.join("out.png").to_string_lossy())
                .is_none()
        );
        assert!(roots.admit(&outside.to_string_lossy()).is_none());
    }
}
