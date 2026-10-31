//! The Markdown view of memory: what `memory export` writes and `memory
//! edit` opens (spec: memory, Memory CLI and editing).
//!
//! One `#` section per target and one `##` section per entry, headed by its
//! id (`## new` for an entry to create), with the entry's settings in an
//! HTML comment so the rendered page shows only the text:
//!
//! ```text
//! # USER PROFILE (who the user is)
//!
//! ## m_abcdefghijkl
//! <!-- kind=preference tier=core pinned=false -->
//! Prefers metric units.
//! ```

use std::fmt::Write as _;

use pmt_store::{MemoryEntry, MemoryKind, MemoryStatus, MemoryTarget, MemoryTier};

use crate::snapshot::heading;

/// One entry section of an edited view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The target section it sits in.
    pub target: MemoryTarget,
    /// The entry's id; `None` for `## new`.
    pub id: Option<String>,
    /// Its kind (default `fact`).
    pub kind: MemoryKind,
    /// Its tier (default `core`).
    pub tier: MemoryTier,
    /// Pinned (default false).
    pub pinned: bool,
    /// The text.
    pub content: String,
}

/// The view of `entries` in `targets`, in target order.
#[must_use]
pub fn render(entries: &[MemoryEntry], targets: &[MemoryTarget]) -> String {
    let mut out = String::new();
    for &target in targets {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "# {}", heading(target));
        for e in entries.iter().filter(|e| e.target == target) {
            let status = match e.status {
                MemoryStatus::Active => String::new(),
                other => format!(" status={other}"),
            };
            let _ = write!(
                out,
                "\n## {}\n<!-- kind={} tier={} pinned={}{status} -->\n{}\n",
                e.id, e.kind, e.tier, e.pinned, e.content
            );
        }
    }
    out
}

fn target_of(heading_line: &str) -> Option<MemoryTarget> {
    let title = heading_line.strip_prefix("# ")?.trim();
    MemoryTarget::ALL
        .iter()
        .copied()
        .find(|&t| title == heading(t) || title.eq_ignore_ascii_case(t.as_str()))
}

/// The entry a `## ` heading opens.
enum Heading {
    /// `## new`.
    New,
    /// `## <id>`.
    Entry(String),
}

fn entry_heading(line: &str) -> Option<Heading> {
    let name = line.strip_prefix("## ")?.trim();
    if name == "new" {
        Some(Heading::New)
    } else if crate::id::is_entry(name) {
        Some(Heading::Entry(name.to_owned()))
    } else {
        None
    }
}

/// Applies `key=value` settings of a `<!-- ... -->` line to `section`.
fn settings(line: &str, section: &mut Section) -> Result<(), String> {
    let body = line
        .trim()
        .strip_prefix("<!--")
        .and_then(|l| l.strip_suffix("-->"))
        .unwrap_or_default();
    for pair in body.split_whitespace() {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("unreadable setting '{pair}' (use key=value)"))?;
        let bad = |_| format!("invalid {key} '{value}'");
        match key {
            "kind" => section.kind = value.parse().map_err(bad)?,
            "tier" => section.tier = value.parse().map_err(bad)?,
            "pinned" => {
                section.pinned = value
                    .parse()
                    .map_err(|_| format!("invalid pinned '{value}'"))?;
            }
            // Shown for information; the status is not edited here.
            "status" => {}
            _ => return Err(format!("unknown setting '{key}'")),
        }
    }
    Ok(())
}

/// The state of [`parse`] between lines.
#[derive(Default)]
struct Parser {
    target: Option<MemoryTarget>,
    sections: Vec<Section>,
    open: bool,
}

impl Parser {
    fn line(&mut self, line: &str) -> Result<(), String> {
        if let Some(t) = target_of(line) {
            self.target = Some(t);
            self.open = false;
            return Ok(());
        }
        if let Some(heading) = entry_heading(line) {
            let target = self
                .target
                .ok_or("an entry before any `# MEMORY` or `# USER PROFILE` heading")?;
            self.sections.push(Section {
                target,
                id: match heading {
                    Heading::New => None,
                    Heading::Entry(id) => Some(id),
                },
                kind: MemoryKind::Fact,
                tier: MemoryTier::Core,
                pinned: false,
                content: String::new(),
            });
            self.open = true;
            return Ok(());
        }
        let Some(section) = self.sections.last_mut().filter(|_| self.open) else {
            return if line.trim().is_empty() {
                Ok(())
            } else {
                Err("text outside an entry section (`## <id>` or `## new`)".to_owned())
            };
        };
        if section.content.is_empty() && line.trim_start().starts_with("<!--") {
            return settings(line, section);
        }
        section.content.push_str(line);
        section.content.push('\n');
        Ok(())
    }
}

/// Reads a view back.
///
/// # Errors
///
/// What is malformed, with its line number.
pub fn parse(text: &str) -> Result<Vec<Section>, String> {
    let mut parser = Parser::default();
    for (n, line) in text.lines().enumerate() {
        parser
            .line(line)
            .map_err(|m| format!("line {}: {m}", n + 1))?;
    }
    let mut sections = parser.sections;
    for section in &mut sections {
        section.content = section.content.trim().to_owned();
    }
    if let Some(empty) = sections.iter().find(|s| s.content.is_empty()) {
        return Err(format!(
            "entry {} is empty (delete the whole section to remove it)",
            empty.id.as_deref().unwrap_or("new")
        ));
    }
    Ok(sections)
}

/// The unified diff from view `before` to view `after`.
#[must_use]
pub fn diff(before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .header("memory (before)", "memory (after)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use pmt_store::MemoryOrigin;

    use super::*;

    fn entry(id: &str, target: MemoryTarget, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            target,
            kind: MemoryKind::Preference,
            tier: MemoryTier::Core,
            content: content.into(),
            pinned: true,
            status: MemoryStatus::Active,
            expires_at: None,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            last_recalled_at: None,
            recall_count: 0,
            origin: MemoryOrigin::User,
            origin_session: None,
        }
    }

    #[test]
    fn a_rendered_view_reads_back() {
        let entries = [
            entry(
                "m_aaaaaaaaaaaa",
                MemoryTarget::Memory,
                "Repo uses just.\nRun `just check`.",
            ),
            entry(
                "m_bbbbbbbbbbbb",
                MemoryTarget::User,
                "Prefers metric units.",
            ),
        ];
        let text = render(&entries, MemoryTarget::ALL);
        assert!(text.contains("## m_bbbbbbbbbbbb\n<!-- kind=preference tier=core pinned=true -->\nPrefers metric units.\n"), "{text}");
        let sections = parse(&text).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].content, "Repo uses just.\nRun `just check`.");
        assert_eq!(sections[1].target, MemoryTarget::User);
        assert!(sections[1].pinned);
    }

    #[test]
    fn new_sections_and_errors() {
        let sections = parse("# USER PROFILE (who the user is)\n\n## new\nLikes tea.\n").unwrap();
        assert_eq!(sections[0].id, None);
        assert_eq!(sections[0].kind, MemoryKind::Fact);
        assert!(parse("## new\nx\n").unwrap_err().contains("line 1"));
        assert!(
            parse("# user\n## new\n<!-- kind=habit -->\nx\n")
                .unwrap_err()
                .contains("invalid kind")
        );
        assert!(parse("# user\n## new\n\n").unwrap_err().contains("empty"));
    }
}
