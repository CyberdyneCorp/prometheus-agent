//! `prometheus skills config` (spec: skills, Interactive per-surface skill
//! enablement): every visible skill with its enabled state, globally and
//! per surface or adapter; each answer toggles one skill globally
//! (`skills.disabled`) or on one platform
//! (`skills.platform_disabled.<platform>`). Changes apply from the next
//! session; the essential skill cannot be disabled.

use std::collections::{BTreeMap, BTreeSet};

use pmt_core::error::{Error, codes};
use pmt_skills::activation;

use super::Env;
use crate::setup_cmd::Wizard;

/// The disabled state being edited.
struct State {
    global: BTreeSet<String>,
    platforms: BTreeMap<String, BTreeSet<String>>,
}

impl State {
    fn line(&self, n: usize, name: &str) -> String {
        let mark = if self.global.contains(name) {
            "off"
        } else {
            "on "
        };
        let off: Vec<&str> = self
            .platforms
            .iter()
            .filter(|(_, names)| names.contains(name))
            .map(|(p, _)| p.as_str())
            .collect();
        let per = if off.is_empty() {
            String::new()
        } else {
            format!("  (off on {})", off.join(", "))
        };
        format!("{n:>3}) [{mark}] {name}{per}")
    }

    fn disabled(&self, name: &str, platform: Option<&str>) -> bool {
        match platform {
            None => self.global.contains(name),
            Some(p) => self.platforms.get(p).is_some_and(|s| s.contains(name)),
        }
    }

    fn set(&mut self, name: &str, platform: Option<&str>, disabled: bool) {
        let set = match platform {
            None => &mut self.global,
            Some(p) => self.platforms.entry(p.to_owned()).or_default(),
        };
        if disabled {
            set.insert(name.to_owned());
        } else {
            set.remove(name);
        }
    }
}

/// `prometheus skills config`.
///
/// # Errors
///
/// `cli.usage` (exit 2) without a terminal; config write errors.
pub(super) fn config(env: &Env) -> Result<u8, Error> {
    if !crate::prompt::stdin_is_tty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "skills config needs an interactive terminal; use `prometheus skills enable|disable <name> [--platform P]`",
        ));
    }
    let skills = &env.scope.config().skills;
    let mut state = State {
        global: skills.disabled.iter().cloned().collect(),
        platforms: skills
            .platform_disabled
            .iter()
            .map(|(p, names)| (p.clone(), names.iter().cloned().collect()))
            .collect(),
    };
    let names: Vec<String> = env
        .profile_catalog()
        .skills
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let mut wizard = Wizard::stdio();
    wizard.say("Skills (on/off everywhere, then the platforms where they are off):")?;
    for (n, name) in names.iter().enumerate() {
        wizard.say(&state.line(n + 1, name))?;
    }
    while let Some(answer) = wizard.ask("toggle <number|name> [platform] (Enter to finish):")? {
        let mut words = answer.split_whitespace();
        let target = words.next().unwrap_or_default();
        let platform = words.next();
        let index = target.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
        let Some(name) = index
            .and_then(|i| names.get(i))
            .or_else(|| names.iter().find(|n| *n == target))
        else {
            wizard.say(&format!("no skill '{target}'"))?;
            continue;
        };
        let disable = !state.disabled(name, platform);
        let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
        match activation::set_disabled(&*english, env.scope.profile(), name, platform, disable) {
            Ok(message) => {
                state.set(name, platform, disable);
                wizard.say(&message)?;
            }
            Err(e) => wizard.say(e.message())?,
        }
    }
    wizard.say("Changes apply from the next session.")?;
    Ok(0)
}
