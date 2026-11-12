//! `/journey` (aliases `/learning`, `/memory-graph`; spec: memory,
//! Learning journey timeline; Journey pruning and editing) in any chat:
//! the timeline, `list`, `delete <node> [--yes]` and `edit <node>
//! [text]`. A chat has no editor, so an edit takes the new text inline
//! (the terminal's `prometheus journey edit` opens `$EDITOR`), and a
//! delete without `--yes` only says what it would do.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::JourneyNode;
use pmt_store::Store;

use super::nodes::{self, Node};
use super::prune::Journey;
use super::render::{self, Style};

/// The usage line.
pub const USAGE: &str = "usage: /journey [list | delete <node> [--yes] | edit <node> <new text>] (node: skill:<name> or m_...)";

/// A chat's timeline: no constellation, no color.
const CHAT: Style = Style {
    width: 120,
    height: 0,
    color: false,
};

/// Runs `/journey <args>`; the reply.
///
/// # Errors
///
/// Unknown nodes, refused changes, store errors.
pub fn run(
    scope: &ProfileScope,
    store: &Arc<Store>,
    user_home: Option<&Path>,
    args: &str,
) -> Result<String, Error> {
    let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
    let journey = Journey {
        scope,
        store,
        user_home,
        text: &*strings,
    };
    let args = args.trim();
    let (verb, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let nodes = || nodes::list(scope, store, user_home, None).map(|l| l.nodes);
    match verb {
        "" => Ok(render::view_in(journey.text, &nodes()?, CHAT)
            .trim_end()
            .to_owned()),
        "list" => Ok(listing(journey.text, &nodes()?).trim_end().to_owned()),
        "delete" => delete(&journey, rest),
        "edit" => edit(&journey, rest),
        _ => Err(Error::new(&codes::CLI_USAGE, USAGE)),
    }
}

fn delete(journey: &Journey<'_>, rest: &str) -> Result<String, Error> {
    let mut words = rest.split_whitespace();
    let node = Node::parse(words.next().unwrap_or_default())?;
    let yes = words.any(|w| w == "--yes");
    if !yes {
        let what = journey.describe(&node)?;
        let id = rest.split_whitespace().next().unwrap_or_default();
        return Ok(journey.text.text(
            "slash.journey.confirm_delete",
            &[("what", &what), ("id", &id)],
        ));
    }
    journey.delete(&node)
}

fn edit(journey: &Journey<'_>, rest: &str) -> Result<String, Error> {
    let (id, text) = rest
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((rest.trim(), ""));
    let node = Node::parse(id)?;
    if text.trim().is_empty() {
        let current = journey.text(&node)?;
        let hint = journey.text.text("slash.journey.edit_hint", &[("id", &id)]);
        return Ok(format!("{}\n\n{hint}", current.trim_end()));
    }
    journey.apply(&node, text)
}

/// One line per node: id, kind, origin, created, title.
#[must_use]
pub fn listing(text: &dyn pmt_core::text::Catalog, nodes: &[JourneyNode]) -> String {
    if nodes.is_empty() {
        return format!("{}\n", text.text(render::EMPTY, &[]));
    }
    let width = nodes.iter().map(|n| n.id.len()).max().unwrap_or(0);
    let mut out = String::new();
    for node in nodes {
        let created = node.created_at.get(..10).unwrap_or(&node.created_at);
        let _ = writeln!(
            out,
            "{:<width$}  {:<6}  {:<17}  {created}  {}",
            node.id, node.kind, node.origin, node.title
        );
    }
    out
}
