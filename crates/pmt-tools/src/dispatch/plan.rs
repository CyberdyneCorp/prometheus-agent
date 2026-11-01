//! Dependency planning of one model response's tool calls (spec:
//! agent-turn-loop, Dependency-planned parallel tool batches; Undeclared
//! tools are sequential).
//!
//! Call B depends on an earlier call A when they conflict: a write and any
//! access to an overlapping path, the same named exclusive resource, either
//! call `interactive`, undeclared (no resource claims) or with arguments
//! that do not parse. Calls whose dependencies are done run concurrently.
//!
//! Paths are compared lexically (`.` and `..` folded, no filesystem
//! access): every call of a batch shares the session's working directory,
//! so two relative paths compare as they are. A relative path against an
//! absolute or `~` one cannot be decided without the working directory and
//! counts as overlapping, which only ever costs parallelism.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::ToolCall;
use crate::descriptor::{PathClaim, ResourceClaims};
use crate::files::patch::v4a::touched_paths;
use crate::surface::ToolSurface;

/// The plan of one batch: for each call, the earlier calls it waits for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatchPlan {
    deps: Vec<Vec<usize>>,
}

impl BatchPlan {
    /// Plans `calls` against the session's `surface`. A name the surface
    /// does not offer fails without touching anything, so it waits for
    /// nothing and blocks nothing.
    #[must_use]
    pub fn of(calls: &[ToolCall], surface: &ToolSurface) -> Self {
        let footprints: Vec<Footprint> = calls
            .iter()
            .map(|call| match surface.get(&call.name) {
                None => Footprint::Free,
                Some(tool) => Footprint::of(tool.descriptor.claims.as_ref(), &call.arguments),
            })
            .collect();
        let deps = (0..footprints.len())
            .map(|b| {
                (0..b)
                    .filter(|&a| footprints[a].conflicts(&footprints[b]))
                    .collect()
            })
            .collect();
        Self { deps }
    }

    /// The calls `call` waits for (indices of earlier calls).
    #[must_use]
    pub fn deps(&self, call: usize) -> &[usize] {
        self.deps.get(call).map_or(&[], Vec::as_slice)
    }

    /// Every edge `(from, to)`: call `to` waits for call `from`.
    pub fn edges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.deps
            .iter()
            .enumerate()
            .flat_map(|(to, deps)| deps.iter().map(move |&from| (from, to)))
    }

    /// Calls planned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.deps.len()
    }

    /// Whether the plan has no call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }
}

/// What one call touches, as the planner sees it.
#[derive(Debug)]
enum Footprint {
    /// Runs alone: interactive, undeclared or unreadable arguments.
    Barrier,
    /// Touches nothing (an unknown tool).
    Free,
    /// Declared paths and resources.
    Claims {
        reads: Vec<PathBuf>,
        writes: Vec<PathBuf>,
        exclusive: Vec<String>,
    },
}

impl Footprint {
    fn of(claims: Option<&ResourceClaims>, arguments: &str) -> Self {
        let Some(claims) = claims.filter(|c| !c.interactive) else {
            return Self::Barrier;
        };
        let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
            return Self::Barrier;
        };
        Self::Claims {
            reads: paths(&claims.reads, &arguments),
            writes: paths(&claims.writes, &arguments),
            exclusive: claims.exclusive.clone(),
        }
    }

    fn conflicts(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Barrier, _) | (_, Self::Barrier) => true,
            (Self::Free, _) | (_, Self::Free) => false,
            (
                Self::Claims {
                    reads: ra,
                    writes: wa,
                    exclusive: xa,
                },
                Self::Claims {
                    reads: rb,
                    writes: wb,
                    exclusive: xb,
                },
            ) => {
                xa.iter().any(|x| xb.contains(x))
                    || any_overlap(wa, rb.iter().chain(wb))
                    || any_overlap(wb, ra.iter())
            }
        }
    }
}

fn any_overlap<'a>(writes: &[PathBuf], others: impl Iterator<Item = &'a PathBuf>) -> bool {
    let others: Vec<&PathBuf> = others.collect();
    writes.iter().any(|w| others.iter().any(|o| overlaps(w, o)))
}

/// The lexical paths `claims` name for `arguments`.
fn paths(claims: &[PathClaim], arguments: &Value) -> Vec<PathBuf> {
    let text = |pointer: &str| arguments.pointer(pointer).and_then(Value::as_str);
    claims
        .iter()
        .flat_map(|claim| match claim {
            PathClaim::Argument(pointer) => text(pointer).map(str::to_owned).into_iter().collect(),
            PathClaim::ArgumentOrCwd(pointer) => vec![text(pointer).unwrap_or(".").to_owned()],
            PathClaim::PatchText(pointer) => text(pointer).map(touched_paths).unwrap_or_default(),
            PathClaim::Fixed(glob) => vec![literal_prefix(glob)],
        })
        .map(|path| normalize(Path::new(&path)))
        .collect()
}

/// The part of a glob before its first wildcard component.
fn literal_prefix(glob: &str) -> String {
    let wild = |c: &str| c.contains(['*', '?', '[', '{']);
    let kept: Vec<&str> = glob.split('/').take_while(|c| !wild(c)).collect();
    if kept.is_empty() || kept == [""] {
        ".".to_owned()
    } else {
        kept.join("/")
    }
}

/// `path` with `.` dropped and `..` folded where possible.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether one path contains the other. Different anchors (relative
/// against absolute or `~`) are undecidable here and count as overlapping.
fn overlaps(a: &Path, b: &Path) -> bool {
    if anchor(a) != anchor(b) {
        return true;
    }
    a.starts_with(b) || b.starts_with(a)
}

#[derive(PartialEq, Eq)]
enum Anchor {
    Root,
    Home,
    Relative,
}

fn anchor(path: &Path) -> Anchor {
    if path.has_root() {
        Anchor::Root
    } else if path.starts_with("~") {
        Anchor::Home
    } else {
        Anchor::Relative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(reads: &[&str], writes: &[&str]) -> Footprint {
        Footprint::Claims {
            reads: reads.iter().map(|p| normalize(Path::new(p))).collect(),
            writes: writes.iter().map(|p| normalize(Path::new(p))).collect(),
            exclusive: Vec::new(),
        }
    }

    #[test]
    fn reads_never_conflict_and_writes_conflict_with_overlapping_access() {
        assert!(!claims(&["a"], &[]).conflicts(&claims(&["a"], &[])));
        assert!(claims(&["src/x.rs"], &[]).conflicts(&claims(&[], &["./src/x.rs"])));
        assert!(claims(&[], &["src"]).conflicts(&claims(&["src/lib/x.rs"], &[])));
        assert!(!claims(&["README.md"], &[]).conflicts(&claims(&[], &["src/x.rs"])));
        assert!(claims(&["a/../b"], &[]).conflicts(&claims(&[], &["b"])));
    }

    #[test]
    fn undecidable_anchors_count_as_overlapping() {
        assert!(claims(&["/repo/x"], &[]).conflicts(&claims(&[], &["x"])));
        assert!(claims(&["~/x"], &[]).conflicts(&claims(&[], &["/x"])));
        assert!(!claims(&["/repo/x"], &[]).conflicts(&claims(&[], &["/repo/y"])));
    }

    #[test]
    fn barriers_conflict_with_everything_but_unknown_tools() {
        assert!(Footprint::Barrier.conflicts(&claims(&["a"], &[])));
        assert!(Footprint::Free.conflicts(&Footprint::Barrier));
        assert!(!Footprint::Free.conflicts(&claims(&[], &["a"])));
    }

    #[test]
    fn interactive_undeclared_and_unparseable_calls_are_barriers() {
        let interactive = ResourceClaims {
            interactive: true,
            ..ResourceClaims::none()
        };
        assert!(matches!(
            Footprint::of(Some(&interactive), "{}"),
            Footprint::Barrier
        ));
        assert!(matches!(Footprint::of(None, "{}"), Footprint::Barrier));
        assert!(matches!(
            Footprint::of(Some(&ResourceClaims::none()), "{not json"),
            Footprint::Barrier
        ));
    }

    #[test]
    fn glob_claims_keep_their_literal_prefix() {
        assert_eq!(literal_prefix("/etc/*.conf"), "/etc");
        assert_eq!(literal_prefix("**/x"), ".");
        assert_eq!(literal_prefix("a/b"), "a/b");
    }
}
