//! The single typed slash-command registry (spec: cli, Single typed
//! slash-command registry; Command resolution; Precedence for
//! non-built-in slash commands; Built-in slash command set).
//!
//! Every built-in command is declared once in [`builtins::BUILTINS`] with
//! its aliases, category, typed arguments, surfaces, busy policy and
//! effect; `/help` is generated from it. Quick commands, plugin commands
//! and skill commands register on top ([`Registry::register`]) in that
//! precedence order; a plugin that redeclares a built-in fails to load.

pub mod builtins;
mod resolve;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_core::error::{Error, codes};

pub use resolve::{Resolution, split as split_line};

/// Where a command can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Surface {
    /// The line interface.
    Cli,
    /// The full-screen terminal UI.
    Tui,
    /// Messaging platforms.
    Gateway,
    /// The desktop app.
    Desktop,
    /// The web dashboard.
    Dashboard,
    /// ACP clients.
    Acp,
}

/// Every surface.
pub const ALL_SURFACES: &[Surface] = &[
    Surface::Cli,
    Surface::Tui,
    Surface::Gateway,
    Surface::Desktop,
    Surface::Dashboard,
    Surface::Acp,
];

/// The interactive terminal surfaces.
pub const TERMINALS: &[Surface] = &[Surface::Cli, Surface::Tui, Surface::Desktop];

/// What a command typed while a turn runs does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Refused with `/<name> is unavailable while the agent is working`.
    Reject,
    /// Runs after the turn.
    Queue,
    /// Stops the turn, then runs.
    InterruptThenDispatch,
    /// Runs at once, alongside the turn.
    Immediate,
}

/// When a command's change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Changes nothing about the session.
    None,
    /// Applies now.
    Immediate,
    /// Persisted; applies to the next session unless `--now`.
    NextSession,
    /// Starts a new session.
    NewSession,
}

/// Groups of `/help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Session lifecycle and history.
    Session,
    /// Model and reasoning.
    Model,
    /// Tools, skills and memory.
    Tools,
    /// Configuration and appearance.
    Config,
    /// Information.
    Info,
    /// Leaving.
    Exit,
}

impl Category {
    fn title(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Model => "Model",
            Self::Tools => "Tools, skills and memory",
            Self::Config => "Configuration and display",
            Self::Info => "Information",
            Self::Exit => "Exit",
        }
    }
}

/// The type of one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// Free text, case kept (the rest of the line).
    Text,
    /// One word.
    Word,
    /// One of a fixed set.
    Choice(&'static [&'static str]),
    /// A `--flag`.
    Flag,
}

/// One declared argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg {
    /// Its name in the usage line.
    pub name: &'static str,
    /// Its type.
    pub kind: ArgKind,
    /// Whether it must be given.
    pub required: bool,
}

/// One built-in command.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    /// Name without `/`.
    pub name: &'static str,
    /// Other names.
    pub aliases: &'static [&'static str],
    /// `/help` group.
    pub category: Category,
    /// One line for `/help`.
    pub description: &'static str,
    /// Arguments.
    pub args: &'static [Arg],
    /// Where it runs.
    pub surfaces: &'static [Surface],
    /// While a turn runs.
    pub busy: BusyPolicy,
    /// When it takes effect.
    pub effect: Effect,
}

impl Command {
    /// Whether the command runs on `surface`.
    #[must_use]
    pub fn on(&self, surface: Surface) -> bool {
        self.surfaces.contains(&surface)
    }

    /// `/name <arg> [arg]`.
    #[must_use]
    pub fn usage(&self) -> String {
        let mut usage = format!("/{}", self.name);
        for arg in self.args {
            let shown = match arg.kind {
                ArgKind::Choice(values) => values.join("|"),
                ArgKind::Flag => format!("--{}", arg.name),
                ArgKind::Text | ArgKind::Word => format!("<{}>", arg.name),
            };
            let _ = if arg.required {
                write!(usage, " {shown}")
            } else {
                write!(usage, " [{shown}]")
            };
        }
        usage
    }
}

/// Where a non-built-in command comes from, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `[quick_commands.<name>]`.
    Quick,
    /// A plugin.
    Plugin,
    /// A skill or bundle.
    Skill,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick command",
            Self::Plugin => "plugin command",
            Self::Skill => "skill",
        }
    }
}

/// A command declared by a plugin, skill or quick command, with its enum
/// values as text (validated on registration).
#[derive(Debug, Clone, Default)]
pub struct Declaration {
    /// Name without `/`.
    pub name: String,
    /// Other names.
    pub aliases: Vec<String>,
    /// One line for `/help`.
    pub description: String,
    /// Surface names (empty: every surface).
    pub surfaces: Vec<String>,
    /// `reject` (default), `queue`, `interrupt_then_dispatch` or `immediate`.
    pub busy_policy: Option<String>,
    /// `none` (default), `immediate`, `next_session` or `new_session`.
    pub effect: Option<String>,
}

/// A registered non-built-in command.
#[derive(Debug, Clone)]
pub struct External {
    /// Name.
    pub name: String,
    /// Other names.
    pub aliases: Vec<String>,
    /// One line for `/help`.
    pub description: String,
    /// Where it runs.
    pub surfaces: Vec<Surface>,
    /// While a turn runs.
    pub busy: BusyPolicy,
    /// When it takes effect.
    pub effect: Effect,
    /// Where it comes from.
    pub source: Source,
}

/// The commands of one session: the built-ins and what registered.
#[derive(Debug, Clone)]
pub struct Registry {
    builtins: &'static [Command],
    external: BTreeMap<String, External>,
    /// Load-time warnings (same name from two non-built-in sources).
    pub warnings: Vec<String>,
}

impl Registry {
    /// The built-in commands.
    ///
    /// # Errors
    ///
    /// `slash.invalid` when two built-ins share a name or alias.
    pub fn new() -> Result<Self, Error> {
        Self::with_builtins(builtins::BUILTINS)
    }

    /// A registry over `builtins` (tests).
    ///
    /// # Errors
    ///
    /// `slash.invalid` when two entries share a name or alias.
    pub fn with_builtins(builtins: &'static [Command]) -> Result<Self, Error> {
        let mut seen = BTreeMap::new();
        for command in builtins {
            for name in std::iter::once(&command.name).chain(command.aliases) {
                if let Some(first) = seen.insert(*name, command.name) {
                    return Err(Error::new(
                        &codes::SLASH_INVALID,
                        format!(
                            "slash command '/{name}' is declared by both /{first} and /{}",
                            command.name
                        ),
                    ));
                }
            }
        }
        Ok(Self {
            builtins,
            external: BTreeMap::new(),
            warnings: Vec::new(),
        })
    }

    /// The built-in entry named `name` (no alias lookup).
    #[must_use]
    pub fn builtin(&self, name: &str) -> Option<&'static Command> {
        self.builtins.iter().find(|c| c.name == name)
    }

    /// Every built-in name and alias (for skill slug collisions).
    #[must_use]
    pub fn builtin_names(&self) -> Vec<&'static str> {
        self.builtins
            .iter()
            .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
            .collect()
    }

    /// Registers a quick, plugin or skill command. A name that is a
    /// built-in fails for a plugin (`slash command '/model' conflicts with
    /// built-in`) and is skipped with a warning for the other sources; a
    /// name two non-built-in sources share keeps the higher-precedence one
    /// and records one warning naming both.
    ///
    /// # Errors
    ///
    /// `slash.conflict` (a plugin redeclaring a built-in) and
    /// `slash.invalid`-style enum errors (`config.invalid`).
    pub fn register(&mut self, declaration: &Declaration, source: Source) -> Result<(), Error> {
        let command = parse(declaration, source)?;
        let builtins = self.builtin_names();
        for name in std::iter::once(&command.name).chain(&command.aliases) {
            if builtins.contains(&name.as_str()) {
                let message = format!("slash command '/{name}' conflicts with built-in");
                if source == Source::Plugin {
                    return Err(Error::new(&codes::SLASH_CONFLICT, message));
                }
                self.warnings
                    .push(format!("{message}; the {} is ignored", source.label()));
                return Ok(());
            }
        }
        match self.external.get(&command.name) {
            Some(existing) => {
                let (kept, lost) = if existing.source <= source {
                    (existing.source, source)
                } else {
                    (source, existing.source)
                };
                self.warnings.push(format!(
                    "slash command '/{}' is defined by a {} and a {}; the {} wins",
                    command.name,
                    kept.label(),
                    lost.label(),
                    kept.label()
                ));
                if source < existing.source {
                    self.external.insert(command.name.clone(), command);
                }
            }
            None => {
                self.external.insert(command.name.clone(), command);
            }
        }
        Ok(())
    }

    /// The registered command named `name`.
    #[must_use]
    pub fn external(&self, name: &str) -> Option<&External> {
        self.external.get(name)
    }

    /// `/help` for `surface`: the commands available there, grouped by
    /// category, then the registered ones.
    #[must_use]
    pub fn help(&self, surface: Surface) -> String {
        let mut groups: BTreeMap<Category, Vec<&Command>> = BTreeMap::new();
        for command in self.builtins.iter().filter(|c| c.on(surface)) {
            groups.entry(command.category).or_default().push(command);
        }
        let mut text = String::new();
        for (category, commands) in groups {
            let _ = writeln!(text, "{}:", category.title());
            for command in commands {
                let aliases: Vec<String> =
                    command.aliases.iter().map(|a| format!("/{a}")).collect();
                let aliases = if aliases.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", aliases.join(", "))
                };
                let _ = writeln!(
                    text,
                    "  {:<28} {}{aliases}",
                    command.usage(),
                    command.description
                );
            }
        }
        let externals: Vec<&External> = self
            .external
            .values()
            .filter(|e| e.surfaces.contains(&surface))
            .collect();
        if !externals.is_empty() {
            text.push_str("Your commands:\n");
            for external in externals {
                let _ = writeln!(
                    text,
                    "  /{:<27} {} ({})",
                    external.name,
                    external.description,
                    external.source.label()
                );
            }
        }
        text
    }
}

fn parse(declaration: &Declaration, source: Source) -> Result<External, Error> {
    let invalid = |what: &str, value: &str| {
        Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "slash command '/{}': unknown {what} '{value}'",
                declaration.name
            ),
        )
    };
    let busy = match declaration.busy_policy.as_deref().unwrap_or("reject") {
        "reject" => BusyPolicy::Reject,
        "queue" => BusyPolicy::Queue,
        "interrupt_then_dispatch" => BusyPolicy::InterruptThenDispatch,
        "immediate" => BusyPolicy::Immediate,
        other => return Err(invalid("busy_policy", other)),
    };
    let effect = match declaration.effect.as_deref().unwrap_or("none") {
        "none" => Effect::None,
        "immediate" => Effect::Immediate,
        "next_session" => Effect::NextSession,
        "new_session" => Effect::NewSession,
        other => return Err(invalid("effect", other)),
    };
    let surfaces = if declaration.surfaces.is_empty() {
        ALL_SURFACES.to_vec()
    } else {
        declaration
            .surfaces
            .iter()
            .map(|s| surface(s).ok_or_else(|| invalid("surface", s)))
            .collect::<Result<_, _>>()?
    };
    Ok(External {
        name: declaration.name.trim_start_matches('/').to_lowercase(),
        aliases: declaration
            .aliases
            .iter()
            .map(|a| a.trim_start_matches('/').to_lowercase())
            .collect(),
        description: declaration.description.clone(),
        surfaces,
        busy,
        effect,
        source,
    })
}

fn surface(name: &str) -> Option<Surface> {
    Some(match name {
        "cli" => Surface::Cli,
        "tui" => Surface::Tui,
        "gateway" => Surface::Gateway,
        "desktop" => Surface::Desktop,
        "dashboard" => Surface::Dashboard,
        "acp" => Surface::Acp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
