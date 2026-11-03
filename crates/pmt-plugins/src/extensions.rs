//! What loaded plugins contribute through the hook world's `extensions`
//! (spec: plugin-system, Plugin system-prompt sections are bounded and
//! frozen; Plugin slash commands).
//!
//! Section ids and commands are registered once per load, in plugin load
//! order. A section needs the `prompt_section` grant, a command the
//! `commands` grant naming it; an ungranted one is skipped with a WARN. A
//! section id or command name another plugin already registered is
//! refused naming that plugin. Section text is asked for only when a
//! session's prompt is built ([`Contributions::render_sections`]), so it is
//! fixed for that session: each must fit the granted size and 4000
//! characters, and sections past 8000 characters in total are dropped in
//! load order, each with a WARN.

use std::sync::Arc;

use pmt_core::capability::{Capability, CapabilitySet, PluginRight};
use pmt_core::error::{Error, codes};

use crate::host::extensions_world::Command;
use crate::host::{CallInfo, Instance};

/// Most characters of one section.
pub const SECTION_MAX_CHARS: usize = 4000;
/// Most characters of every plugin section together.
pub const SECTIONS_TOTAL_CHARS: usize = 8000;

/// A registered system-prompt section.
#[derive(Debug, Clone)]
pub struct Section {
    /// The plugin that renders it.
    pub plugin: String,
    /// Its id.
    pub id: String,
    instance: Arc<Instance>,
}

/// A registered plugin slash command.
#[derive(Debug, Clone)]
pub struct PluginCommand {
    /// The plugin that runs it.
    pub plugin: String,
    /// Name without `/`.
    pub name: String,
    /// One line for `/help`.
    pub description: String,
    /// Surface names (empty: every surface).
    pub surfaces: Vec<String>,
    instance: Arc<Instance>,
}

impl PluginCommand {
    /// Runs `/<name> <args>` (blocking: call it off the async runtime);
    /// the plugin's own error comes back as `Err(text)`.
    ///
    /// # Errors
    ///
    /// `plugins.call` when the plugin traps or times out.
    pub fn run(&self, call: &CallInfo, args: &str) -> Result<Result<String, String>, Error> {
        self.instance.run_command(call, &self.name, args)
    }
}

/// The sections and commands of one load.
#[derive(Debug, Clone, Default)]
pub struct Contributions {
    /// Sections in load order.
    pub sections: Vec<Section>,
    /// Commands in load order.
    pub commands: Vec<PluginCommand>,
}

/// A section id: `^[a-z0-9][a-z0-9._-]{0,127}$`.
#[must_use]
pub fn valid_section_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 128
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

impl Contributions {
    /// Registers `ids` of `instance`'s sections; returns the warnings of
    /// the ones skipped for a missing grant or an invalid id.
    ///
    /// # Errors
    ///
    /// `plugins.load`: `system prompt section '<id>' is already registered
    /// by plugin '<other>'`, naming the plugin itself when it lists `<id>`
    /// twice (nothing of the plugin's sections registers).
    pub fn add_sections(
        &mut self,
        instance: &Arc<Instance>,
        ids: Vec<String>,
        granted: &CapabilitySet,
    ) -> Result<Vec<String>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let plugin = instance.id();
        let any_size = Capability::Plugin {
            right: PluginRight::PromptSection,
            target: Some("1".to_owned()),
        };
        if !granted.allows(&any_size) {
            return Ok(vec![format!(
                "capability_denied: prompt_section; plugin '{plugin}' sections are not registered"
            )]);
        }
        for (at, id) in ids.iter().enumerate() {
            // A plugin listing an id twice collides with itself.
            let owner = match self.sections.iter().find(|s| &s.id == id) {
                Some(section) => Some(section.plugin.as_str()),
                None => ids[..at].contains(id).then_some(plugin),
            };
            if let Some(owner) = owner {
                return Err(Error::new(
                    &codes::PLUGINS_LOAD,
                    format!(
                        "system prompt section '{id}' is already registered by plugin '{owner}'"
                    ),
                ));
            }
        }
        let mut warnings = Vec::new();
        for id in ids {
            if !valid_section_id(&id) {
                warnings.push(format!(
                    "invalid system prompt section id '{id}' (lower-case letters, digits, '.', '_' and '-')"
                ));
                continue;
            }
            self.sections.push(Section {
                plugin: plugin.to_owned(),
                id,
                instance: Arc::clone(instance),
            });
        }
        Ok(warnings)
    }

    /// Registers `instance`'s commands; returns the warnings of the ones
    /// skipped for a missing grant.
    ///
    /// # Errors
    ///
    /// `plugins.load`: `slash command '/<name>' is already registered by
    /// plugin '<other>'`.
    pub fn add_commands(
        &mut self,
        instance: &Arc<Instance>,
        commands: Vec<Command>,
        granted: &CapabilitySet,
    ) -> Result<Vec<String>, Error> {
        let plugin = instance.id();
        let mut warnings = Vec::new();
        for command in commands {
            let name = command.name.trim_start_matches('/').to_lowercase();
            if let Some(owner) = self.commands.iter().find(|c| c.name == name) {
                return Err(Error::new(
                    &codes::PLUGINS_LOAD,
                    format!(
                        "slash command '/{name}' is already registered by plugin '{}'",
                        owner.plugin
                    ),
                ));
            }
            let wanted = Capability::Plugin {
                right: PluginRight::Command,
                target: Some(name.clone()),
            };
            if !granted.allows(&wanted) {
                warnings.push(format!(
                    "capability_denied: commands {name}; '/{name}' is not registered"
                ));
                continue;
            }
            self.commands.push(PluginCommand {
                plugin: plugin.to_owned(),
                name,
                description: command.description,
                surfaces: command.surfaces,
                instance: Arc::clone(instance),
            });
        }
        Ok(warnings)
    }

    /// The command named `name`.
    #[must_use]
    pub fn command(&self, name: &str) -> Option<&PluginCommand> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// Renders every section for a session's prompt (blocking: call it off
    /// the async runtime), dropping with a WARN a section that fails, is
    /// larger than its grant or 4000 characters, or no longer fits the
    /// 8000-character total.
    #[must_use]
    pub fn render_sections(&self, call: &CallInfo) -> Vec<String> {
        let mut total = 0;
        let mut texts = Vec::new();
        for section in &self.sections {
            let Some(text) = render(section, call) else {
                continue;
            };
            let size = text.chars().count();
            if total + size > SECTIONS_TOTAL_CHARS {
                tracing::warn!(
                    plugin = %section.plugin,
                    "system prompt section '{}' dropped: plugin sections exceed {SECTIONS_TOTAL_CHARS} characters",
                    section.id
                );
                continue;
            }
            total += size;
            texts.push(text);
        }
        texts
    }
}

/// One section's text, or `None` (logged) when it cannot be used.
fn render(section: &Section, call: &CallInfo) -> Option<String> {
    let drop = |why: &str| {
        tracing::warn!(
            plugin = %section.plugin,
            "system prompt section '{}' dropped: {why}",
            section.id
        );
    };
    let text = match section.instance.render_section(call, &section.id) {
        Ok(Ok(text)) => text,
        Ok(Err(message)) => {
            drop(&message);
            return None;
        }
        Err(err) => {
            drop(err.message());
            return None;
        }
    };
    let size = text.chars().count();
    let fits = Capability::Plugin {
        right: PluginRight::PromptSection,
        target: Some(size.to_string()),
    };
    let granted = section.instance.context().grants.current();
    if size > SECTION_MAX_CHARS || !granted.allows(&fits) {
        drop(&format!(
            "{size} characters is more than its grant or {SECTION_MAX_CHARS}"
        ));
        return None;
    }
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::valid_section_id;

    #[test]
    fn section_ids_follow_the_pattern() {
        for good in ["style.guide", "a", "0-x_y.z"] {
            assert!(valid_section_id(good), "{good}");
        }
        for bad in ["", "Style", ".x", "a b", &"a".repeat(129)] {
            assert!(!valid_section_id(bad), "{bad}");
        }
    }
}
