//! Load order (spec: plugin-system, Load order, dependencies and
//! deadline): `requires_plugins` dependency order with id-alphabetical
//! tie-breaks. Every member of a dependency cycle is refused naming the
//! cycle; a plugin requiring one that is not among the plugins to load
//! (missing, disabled or refused) is refused with `missing dependency
//! '<id>'`, and so, transitively, is everything that requires it.

use std::collections::{BTreeMap, BTreeSet};

/// One plugin's place in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Load it (its dependencies come earlier in the plan).
    Load(String),
    /// Refuse it with this reason.
    Refuse(String, String),
}

/// The load plan of `plugins` (id -> required ids): loads in dependency
/// order, then the refusals by id.
#[must_use]
pub fn plan(plugins: &BTreeMap<String, Vec<String>>) -> Vec<Step> {
    let mut refused: BTreeMap<String, String> = BTreeMap::new();
    for (id, cycle) in cycles(plugins) {
        refused.insert(id, format!("dependency cycle: {cycle}"));
    }
    let mut steps = Vec::new();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut pending: BTreeSet<&str> = plugins
        .keys()
        .map(String::as_str)
        .filter(|id| !refused.contains_key(*id))
        .collect();
    while let Some(next) = next_ready(plugins, &pending, &placed, &refused) {
        pending.remove(next);
        if let Some(dependency) = missing(plugins, next, &refused) {
            refused.insert(
                next.to_owned(),
                format!("missing dependency '{dependency}'"),
            );
        } else {
            placed.insert(next);
            steps.push(Step::Load(next.to_owned()));
        }
    }
    steps.extend(
        refused
            .into_iter()
            .map(|(id, reason)| Step::Refuse(id, reason)),
    );
    steps
}

/// The first pending plugin, alphabetically, whose dependencies are all
/// placed or settled (refused or not among the plugins).
fn next_ready<'a>(
    plugins: &'a BTreeMap<String, Vec<String>>,
    pending: &BTreeSet<&'a str>,
    placed: &BTreeSet<&str>,
    refused: &BTreeMap<String, String>,
) -> Option<&'a str> {
    pending.iter().copied().find(|id| {
        plugins[*id].iter().all(|dep| {
            placed.contains(dep.as_str()) || refused.contains_key(dep) || !plugins.contains_key(dep)
        })
    })
}

/// A dependency of `id` that is not loadable.
fn missing<'a>(
    plugins: &'a BTreeMap<String, Vec<String>>,
    id: &str,
    refused: &BTreeMap<String, String>,
) -> Option<&'a str> {
    plugins[id]
        .iter()
        .find(|dep| !plugins.contains_key(*dep) || refused.contains_key(*dep))
        .map(String::as_str)
}

/// Every plugin on a dependency cycle, with the cycle as `a -> b -> a`
/// starting from its alphabetically first member.
fn cycles(plugins: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for id in plugins.keys() {
        if found.contains_key(id) {
            continue;
        }
        if let Some(path) = path_back(plugins, id) {
            let rendered = format!("{} -> {id}", path.join(" -> "));
            for member in path {
                found.entry(member).or_insert_with(|| rendered.clone());
            }
        }
    }
    found
}

/// The dependency path from `start` back to itself, if there is one
/// (depth first, dependencies in alphabetical order).
fn path_back(plugins: &BTreeMap<String, Vec<String>>, start: &str) -> Option<Vec<String>> {
    let mut stack = vec![(start.to_owned(), vec![start.to_owned()])];
    let mut visited = BTreeSet::new();
    while let Some((node, path)) = stack.pop() {
        let mut deps: Vec<&String> = plugins.get(&node).map(|d| d.iter().collect())?;
        deps.sort();
        for dep in deps.into_iter().rev() {
            if dep == start {
                return Some(path);
            }
            if plugins.contains_key(dep) && visited.insert(dep.clone()) {
                let mut longer = path.clone();
                longer.push(dep.clone());
                stack.push((dep.clone(), longer));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(id, deps)| {
                (
                    (*id).to_owned(),
                    deps.iter().map(|d| (*d).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn load(id: &str) -> Step {
        Step::Load(id.to_owned())
    }

    fn refuse(id: &str, why: &str) -> Step {
        Step::Refuse(id.to_owned(), why.to_owned())
    }

    #[test]
    fn dependencies_load_first_with_alphabetical_ties() {
        let plan = plan(&graph(&[
            ("a", &["c"]),
            ("b", &[]),
            ("c", &[]),
            ("d", &["a"]),
        ]));
        assert_eq!(plan, vec![load("b"), load("c"), load("a"), load("d")]);
    }

    #[test]
    fn cycle_refuses_only_its_members() {
        // Spec scenario "Cycle refuses only its members".
        let plan = plan(&graph(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]));
        assert_eq!(
            plan,
            vec![
                load("c"),
                refuse("a", "dependency cycle: a -> b -> a"),
                refuse("b", "dependency cycle: a -> b -> a"),
            ]
        );
    }

    #[test]
    fn a_missing_or_refused_dependency_refuses_its_dependents() {
        let plan = plan(&graph(&[
            ("x", &["gone"]),
            ("y", &["x"]),
            ("p", &["p"]),
            ("q", &["p"]),
        ]));
        assert_eq!(
            plan,
            vec![
                refuse("p", "dependency cycle: p -> p"),
                refuse("q", "missing dependency 'p'"),
                refuse("x", "missing dependency 'gone'"),
                refuse("y", "missing dependency 'x'"),
            ]
        );
    }
}
