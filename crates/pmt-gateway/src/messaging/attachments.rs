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
    text: Vec<std::borrow::Cow<'a, str>>,
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
            self.text.push(line.into());
            return;
        }
        if let Some(marker) = fence_marker(trimmed) {
            self.fence = Some(marker);
            self.text.push(line.into());
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
            None if document || voice => self.text.push(rest.into_owned().into()),
            None => self.text.push(line.into()),
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

/// The response-wide modifiers (spec: platform-adapters, Response-wide
/// delivery modifiers and kanban artifacts).
const AS_DOCUMENT: &str = "[[as_document]]";
const AS_VOICE: &str = "[[as_voice]]";

/// A line without its `[[as_document]]` / `[[as_voice]]` modifiers
/// (wherever they stand), and which it carried.
fn modifiers(line: &str) -> (std::borrow::Cow<'_, str>, bool, bool) {
    let document = line.contains(AS_DOCUMENT);
    let voice = line.contains(AS_VOICE);
    if !document && !voice {
        return (std::borrow::Cow::Borrowed(line), false, false);
    }
    let cleaned = line.replace(AS_DOCUMENT, "").replace(AS_VOICE, "");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (std::borrow::Cow::Owned(cleaned), document, voice)
}

/// The mounts of a session whose terminal runs in a container (spec:
/// platform-adapters, Container media path translation): a container path
/// is a host path by its longest mount prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerMounts {
    /// `(container path, host path)` of each volume, the session workspace
    /// mount included.
    pub mounts: Vec<(PathBuf, PathBuf)>,
    /// The container's Prometheus home: nothing under it is translated
    /// unless a mounted cache directory holds it.
    pub home: Option<PathBuf>,
}

impl ContainerMounts {
    /// The host path of container path `path`; `None` when no mount holds
    /// it or it lies in the container's home outside a mounted cache.
    #[must_use]
    pub fn translate(&self, path: &Path) -> Option<PathBuf> {
        let (inside, host) = self
            .mounts
            .iter()
            .filter(|(inside, _)| path.starts_with(inside))
            .max_by_key(|(inside, _)| inside.components().count())?;
        if let Some(home) = &self.home
            && path.starts_with(home)
        {
            let cache =
                inside.starts_with(home) && inside.components().any(|c| c.as_os_str() == "cache");
            if !cache {
                return None;
            }
        }
        Some(host.join(path.strip_prefix(inside).ok()?))
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
    /// The container the session's terminal runs in: its paths are
    /// translated before the checks, and untranslatable ones withheld.
    pub container: Option<ContainerMounts>,
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
            container: None,
        }
    }

    /// The real path of `path` when it may be delivered.
    #[must_use]
    pub fn admit(&self, path: &str) -> Option<PathBuf> {
        let expanded = match (&self.container, path.strip_prefix("~/"), &self.user_home) {
            (Some(container), ..) => container.translate(Path::new(path))?,
            (None, Some(rest), Some(home)) => home.join(rest),
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
    fn a_container_path_is_delivered_as_its_host_file() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("default");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("out")).unwrap();
        std::fs::create_dir_all(profile.join("cache").join("media")).unwrap();
        std::fs::write(workspace.join("out").join("chart.png"), "png").unwrap();
        std::fs::write(profile.join("cache").join("media").join("a.ogg"), "ogg").unwrap();
        let roots = Roots {
            container: Some(ContainerMounts {
                mounts: vec![
                    ("/workspace".into(), workspace.clone()),
                    ("/root/.prometheus".into(), dir.path().join("elsewhere")),
                    (
                        "/root/.prometheus/cache/media".into(),
                        profile.join("cache").join("media"),
                    ),
                ],
                home: Some("/root/.prometheus".into()),
            }),
            ..Roots::of_profile(&profile, Some(&workspace))
        };
        let real = roots.admit("/workspace/out/chart.png").unwrap();
        assert_eq!(
            real,
            std::fs::canonicalize(workspace.join("out/chart.png")).unwrap()
        );
        assert!(roots.admit("/root/.prometheus/cache/media/a.ogg").is_some());
        assert!(roots.admit("/root/.prometheus/secrets.toml").is_none());
        assert!(roots.admit("/etc/passwd").is_none(), "no mount holds it");
        let (kept, notes) = roots.filter(vec![OutboundAttachment::new("/srv/x.png")]);
        assert!(kept.is_empty());
        assert_eq!(notes, ["attachment x.png withheld: path not allowed"]);
    }

    #[test]
    fn modifiers_apply_wherever_they_stand() {
        let (text, attachments) =
            directives("Here it is\n[[as_document]] MEDIA:/ws/photo.jpg\nMEDIA:/ws/b.png");
        assert_eq!(text, "Here it is");
        assert!(attachments.iter().all(|a| a.as_document));
        let (text, _) = directives("Sent [[as_document]] as files");
        assert_eq!(text, "Sent as files");
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
