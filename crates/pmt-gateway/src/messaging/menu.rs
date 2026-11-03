//! Native command menus (spec: platform-adapters, Native command menus
//! and slash registration): which of the profile's commands and skills a
//! capped platform menu shows. Priority entries survive the cap: placed
//! first (`prepend`), last (`append`) or alone (`replace`).

use pmt_config::platforms::{CommandMenu, PriorityMode};

use super::adapter::MenuCommand;

/// The menu of at most `menu.max_commands` entries drawn from `all` (in
/// order) after placing the priority entries.
#[must_use]
pub fn select(all: &[MenuCommand], menu: &CommandMenu) -> Vec<MenuCommand> {
    let cap = menu.max_commands as usize;
    let priority: Vec<MenuCommand> = menu
        .priority
        .iter()
        .filter_map(|name| {
            let name = name.trim_start_matches('/');
            all.iter().find(|c| c.name == name).cloned()
        })
        .take(cap)
        .collect();
    let others: Vec<MenuCommand> = all
        .iter()
        .filter(|c| !priority.contains(c))
        .cloned()
        .collect();
    match menu.priority_mode {
        PriorityMode::Prepend => priority.into_iter().chain(others).take(cap).collect(),
        PriorityMode::Append => {
            let room = cap - priority.len();
            others.into_iter().take(room).chain(priority).collect()
        }
        PriorityMode::Replace => priority,
    }
}

/// The entries of `all` whose name or description contains `term` (case
/// insensitive), in order: the inline picker searches everything, not
/// only what fits the menu.
#[must_use]
pub fn search<'a>(all: &'a [MenuCommand], term: &str) -> Vec<&'a MenuCommand> {
    let term = term.trim().trim_start_matches('/').to_lowercase();
    all.iter()
        .filter(|c| {
            term.is_empty()
                || c.name.to_lowercase().contains(&term)
                || c.description.to_lowercase().contains(&term)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(n: usize) -> Vec<MenuCommand> {
        let mut all: Vec<MenuCommand> = (0..n)
            .map(|i| MenuCommand {
                name: format!("cmd{i:03}"),
                description: format!("command {i}"),
                takes_args: false,
            })
            .collect();
        for name in ["model", "stop", "new"] {
            all.push(MenuCommand {
                name: name.into(),
                description: format!("/{name}"),
                takes_args: false,
            });
        }
        all
    }

    fn menu(mode: PriorityMode) -> CommandMenu {
        CommandMenu {
            max_commands: 60,
            priority: vec!["new".into(), "/stop".into(), "model".into()],
            priority_mode: mode,
        }
    }

    fn names(menu: &[MenuCommand]) -> Vec<&str> {
        menu.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn priority_survives_the_cap() {
        let all = commands(137);
        let menu = select(&all, &menu(PriorityMode::Prepend));
        assert_eq!(menu.len(), 60);
        assert_eq!(names(&menu)[..3], ["new", "stop", "model"]);
        assert_eq!(menu[3].name, "cmd000");
    }

    #[test]
    fn append_and_replace_place_the_priority_entries() {
        let all = commands(137);
        let appended = select(&all, &menu(PriorityMode::Append));
        assert_eq!(appended.len(), 60);
        assert_eq!(names(&appended)[57..], ["new", "stop", "model"]);
        let replaced = select(&all, &menu(PriorityMode::Replace));
        assert_eq!(names(&replaced), ["new", "stop", "model"]);
    }

    #[test]
    fn the_picker_searches_every_entry() {
        let all = commands(137);
        assert_eq!(search(&all, "cmd13").len(), 7);
        assert_eq!(search(&all, "").len(), 140);
        assert_eq!(
            names(
                &search(&all, "/MOD")
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            ),
            ["model"]
        );
    }
}
