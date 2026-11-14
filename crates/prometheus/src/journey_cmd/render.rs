//! The terminal view of the learning journey (spec: memory, Learning
//! journey timeline): a constellation of the nodes laid out by when they
//! were learned (skills `◆`, memories `●`) above the timeline, oldest
//! first. `--play` shows it building up one node per frame.

use std::fmt::Write as _;
use std::hash::{Hash as _, Hasher as _};
use std::io::Write as _;
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::text::Catalog;
use pmt_rpc::contracts::JourneyNode;

/// How the view is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Columns.
    pub width: usize,
    /// Rows of the constellation (0: none).
    pub height: usize,
    /// ANSI colors.
    pub color: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: 80,
            height: 10,
            color: false,
        }
    }
}

/// The catalog key of what a journey with no node says.
pub const EMPTY: &str = "slash.journey.empty";

const SKILL: char = '◆';
const MEMORY: char = '●';

fn mark(node: &JourneyNode) -> char {
    if node.kind == "skill" { SKILL } else { MEMORY }
}

fn paint(text: &str, node: &JourneyNode, color: bool) -> String {
    if !color {
        return text.to_owned();
    }
    let code = if node.kind == "skill" { 36 } else { 33 };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// The whole view of `nodes`, in English (`prometheus journey`).
#[must_use]
pub fn view(nodes: &[JourneyNode], style: Style) -> String {
    view_in(
        &*pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        nodes,
        style,
    )
}

/// The whole view of `nodes`, worded with `text` (`/journey`).
#[must_use]
pub fn view_in(text: &dyn Catalog, nodes: &[JourneyNode], style: Style) -> String {
    if nodes.is_empty() {
        return format!("{}\n", text.text(EMPTY, &[]));
    }
    let skills = nodes.iter().filter(|n| n.kind == "skill").count();
    let memories = nodes.len() - skills;
    let count = |n: usize, one: &str, many: &str| {
        text.text(if n == 1 { one } else { many }, &[("count", &n)])
    };
    let skills = count(
        skills,
        "slash.journey.skill_one",
        "slash.journey.skill_many",
    );
    let memories = count(
        memories,
        "slash.journey.memory_one",
        "slash.journey.memory_many",
    );
    let heading = text.text(
        "slash.journey.heading",
        &[("skills", &skills), ("memories", &memories)],
    );
    let mut out = format!("{heading}\n\n");
    if style.height > 0 {
        out.push_str(&constellation(nodes, style));
        out.push('\n');
    }
    out.push_str(&timeline(nodes, style));
    out
}

/// One line per node: date, mark, kind, title, origin.
#[must_use]
pub fn timeline(nodes: &[JourneyNode], style: Style) -> String {
    let mut out = String::new();
    for node in nodes {
        let date = node.created_at.get(..16).unwrap_or(&node.created_at);
        let line = format!(
            "{} {} {:<6} {}  ({}) {}",
            date.replace('T', " "),
            mark(node),
            node.kind,
            node.title,
            node.origin,
            node.id,
        );
        let line = cut(&line, style.width);
        let _ = writeln!(out, "{}", paint(&line, node, style.color));
    }
    out
}

fn cut(line: &str, width: usize) -> String {
    if line.chars().count() <= width.max(1) {
        return line.to_owned();
    }
    let kept: String = line.chars().take(width.saturating_sub(1).max(1)).collect();
    format!("{kept}…")
}

/// The nodes on a `width` x `height` grid: across by creation time, down
/// by a stable hash of the id.
#[must_use]
pub fn constellation(nodes: &[JourneyNode], style: Style) -> String {
    let (width, height) = (style.width.max(1), style.height.max(1));
    let times: Vec<i64> = nodes.iter().map(seconds).collect();
    let first = times.iter().copied().min().unwrap_or(0);
    let span = (times.iter().copied().max().unwrap_or(0) - first).max(1);
    let mut grid: Vec<Vec<Option<&JourneyNode>>> = vec![vec![None; width]; height];
    for (node, at) in nodes.iter().zip(&times) {
        let x = usize::try_from((at - first) * i64::try_from(width - 1).unwrap_or(0) / span)
            .unwrap_or(0)
            .min(width - 1);
        let y = row_of(&node.id, height);
        grid[y][x] = Some(node);
    }
    let mut out = String::new();
    for row in grid {
        let line: String = row
            .iter()
            .map(|cell| match cell {
                Some(node) => paint(&mark(node).to_string(), node, style.color),
                None => " ".to_owned(),
            })
            .collect();
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

fn seconds(node: &JourneyNode) -> i64 {
    node.created_at
        .parse::<Timestamp>()
        .map_or(0, Timestamp::as_second)
}

fn row_of(id: &str, height: usize) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    usize::try_from(hasher.finish() % u64::try_from(height).unwrap_or(1)).unwrap_or(0)
}

/// The frames of `--play`: the view after each node, in order.
#[must_use]
pub fn frames(nodes: &[JourneyNode], style: Style) -> Vec<String> {
    (1..=nodes.len())
        .map(|n| view(&nodes[..n], style))
        .collect()
}

/// Plays the build-up on a terminal at `fps` frames a second; elsewhere
/// (a pipe, a file) only the last frame is written.
///
/// # Errors
///
/// Writing to stdout.
pub fn play(nodes: &[JourneyNode], style: Style, fps: u32) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    if nodes.is_empty() || !std::io::IsTerminal::is_terminal(&stdout) {
        return stdout.write_all(view(nodes, style).as_bytes());
    }
    let pause = Duration::from_secs(1) / fps.max(1);
    for frame in frames(nodes, style) {
        write!(stdout, "\x1b[2J\x1b[H{frame}")?;
        stdout.flush()?;
        std::thread::sleep(pause);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: &str, at: &str) -> JourneyNode {
        JourneyNode {
            id: id.to_owned(),
            kind: kind.to_owned(),
            title: format!("title of {id}"),
            created_at: at.to_owned(),
            updated_at: at.to_owned(),
            origin: "agent".to_owned(),
            uses: 0,
        }
    }

    fn sample() -> Vec<JourneyNode> {
        vec![
            node("skill:deploy", "skill", "2026-11-01T09:00:00Z"),
            node("m_aaaaaaaaaaaa", "memory", "2026-11-03T09:00:00Z"),
            node("m_bbbbbbbbbbbb", "memory", "2026-11-08T09:00:00Z"),
        ]
    }

    #[test]
    fn the_view_counts_and_lists_every_node_oldest_first() {
        let text = view(&sample(), Style::default());
        assert!(
            text.starts_with("Learning journey: 1 skill, 2 memories"),
            "{text}"
        );
        let deploy = text.find("title of skill:deploy").unwrap();
        let last = text.find("title of m_bbbbbbbbbbbb").unwrap();
        assert!(deploy < last, "{text}");
        assert_eq!(text.matches(SKILL).count(), 2, "mark in grid and timeline");
        assert!(!text.contains('\x1b'), "no color unless asked");
    }

    #[test]
    fn the_first_and_last_nodes_sit_at_the_edges_of_the_grid() {
        let grid = constellation(
            &sample(),
            Style {
                width: 20,
                height: 4,
                color: false,
            },
        );
        assert_eq!(grid.lines().count(), 4);
        let columns: Vec<usize> = grid
            .lines()
            .flat_map(|l| {
                l.chars()
                    .enumerate()
                    .filter(|(_, c)| *c != ' ')
                    .map(|(i, _)| i)
            })
            .collect();
        assert!(columns.contains(&0) && columns.contains(&19), "{grid}");
    }

    #[test]
    fn lines_fit_the_width_and_color_is_optional() {
        let narrow = Style {
            width: 30,
            height: 0,
            color: true,
        };
        let text = view(&sample(), narrow);
        assert!(text.contains("\x1b[36m"), "{text}");
        let plain = timeline(
            &sample(),
            Style {
                color: false,
                ..narrow
            },
        );
        assert!(plain.lines().all(|l| l.chars().count() <= 30), "{plain}");
    }

    #[test]
    fn play_builds_up_one_node_per_frame() {
        let frames = frames(&sample(), Style::default());
        assert_eq!(frames.len(), 3);
        assert!(frames[0].contains("1 skill, 0 memories"), "{}", frames[0]);
        assert!(frames[2].contains("1 skill, 2 memories"), "{}", frames[2]);
    }

    #[test]
    fn an_empty_journey_says_so() {
        let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
        assert_eq!(
            view(&[], Style::default()),
            format!("{}\n", english.get(EMPTY))
        );
    }
}
