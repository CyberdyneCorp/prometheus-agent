//! The built-in slash commands of this build: the cli capability's M1 set
//! and the commands other M1 capabilities define (spec: cli, Built-in
//! slash command set). Commands of later milestones join this table when
//! their owner lands.

use super::{Arg, ArgKind, BusyPolicy, Category, Command, Effect, Surface, TERMINALS};

const fn arg(name: &'static str, kind: ArgKind, required: bool) -> Arg {
    Arg {
        name,
        kind,
        required,
    }
}

const fn text(name: &'static str) -> Arg {
    arg(name, ArgKind::Text, false)
}

const YES: Arg = arg("yes", ArgKind::Flag, false);
const NOW: Arg = arg("now", ArgKind::Flag, false);
const EVERYWHERE: &[Surface] = super::ALL_SURFACES;
const CHATS: &[Surface] = &[
    Surface::Cli,
    Surface::Tui,
    Surface::Gateway,
    Surface::Desktop,
    Surface::Acp,
];

/// Builds one entry.
#[allow(clippy::too_many_arguments)]
const fn command(
    name: &'static str,
    aliases: &'static [&'static str],
    category: Category,
    description: &'static str,
    args: &'static [Arg],
    surfaces: &'static [Surface],
    busy: BusyPolicy,
    effect: Effect,
) -> Command {
    Command {
        name,
        aliases,
        category,
        description,
        args,
        surfaces,
        busy,
        effect,
    }
}

use BusyPolicy::{Immediate, InterruptThenDispatch};
use Category::{Config, Exit, Info, Model, Session, Tools};

/// Every built-in command.
pub static BUILTINS: &[Command] = &[
    command(
        "help",
        &[],
        Info,
        "List the commands available here",
        &[],
        EVERYWHERE,
        Immediate,
        Effect::None,
    ),
    command(
        "new",
        &["reset"],
        Session,
        "End this session and start a new one",
        &[YES],
        CHATS,
        InterruptThenDispatch,
        Effect::NewSession,
    ),
    command(
        "clear",
        &[],
        Session,
        "Clear the screen and start a new session",
        &[YES],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::NewSession,
    ),
    command(
        "stop",
        &[],
        Session,
        "Stop the running turn",
        &[],
        CHATS,
        InterruptThenDispatch,
        Effect::Immediate,
    ),
    command(
        "undo",
        &[],
        Session,
        "Remove the last exchange",
        &[YES],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "retry",
        &[],
        Session,
        "Send the last message again, replacing the answer",
        &[],
        CHATS,
        InterruptThenDispatch,
        Effect::Immediate,
    ),
    command(
        "rewind",
        &[],
        Session,
        "List recent messages, or go back to before message N",
        &[arg("n", ArgKind::Word, false), YES],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "fork",
        &[],
        Session,
        "Continue in a copy of this session",
        &[text("title")],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::NewSession,
    ),
    command(
        "resume",
        &[],
        Session,
        "Switch to another session (id, prefix or title)",
        &[arg("session", ArgKind::Text, true)],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::NewSession,
    ),
    command(
        "sessions",
        &[],
        Session,
        "List recent sessions here; all for every session, search <query>",
        &[text("all | search <query>")],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "title",
        &[],
        Session,
        "Show or set this session's title",
        &[text("title")],
        CHATS,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "save",
        &[],
        Session,
        "Export this session to a file",
        &[
            arg("format", ArgKind::Choice(&["json", "md", "html"]), false),
            text("file"),
        ],
        TERMINALS,
        Immediate,
        Effect::None,
    ),
    command(
        "compress",
        &["compact"],
        Session,
        "Summarize older context (--preview, here [N], or a focus)",
        &[text("focus")],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "usage",
        &[],
        Info,
        "Tokens and cost of this session and its lineage",
        &[],
        EVERYWHERE,
        Immediate,
        Effect::None,
    ),
    command(
        "budget",
        &[],
        Info,
        "Show budgets, or set one",
        &[text("set <scope> <usd>")],
        EVERYWHERE,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "snapshot",
        &["snap"],
        Session,
        "Create, list, restore or prune state snapshots",
        &[text("action")],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "model",
        &[],
        Model,
        "Show the model, or switch it",
        &[text("[provider/]model")],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "reasoning",
        &[],
        Model,
        "Show or set the reasoning effort",
        &[text("level")],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "personality",
        &[],
        Model,
        "Show or set the personality",
        &[text("name"), NOW],
        CHATS,
        BusyPolicy::Reject,
        Effect::NextSession,
    ),
    command(
        "tools",
        &[],
        Tools,
        "List tools, or enable/disable a toolset",
        &[
            arg(
                "action",
                ArgKind::Choice(&["list", "enable", "disable"]),
                false,
            ),
            arg("toolset", ArgKind::Word, false),
            NOW,
        ],
        CHATS,
        BusyPolicy::Reject,
        Effect::NextSession,
    ),
    command(
        "toolsets",
        &[],
        Tools,
        "List toolsets and which are active",
        &[],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "skills",
        &[],
        Tools,
        "List, enable or disable skills",
        &[text("action")],
        CHATS,
        BusyPolicy::Reject,
        Effect::NextSession,
    ),
    command(
        "bundles",
        &[],
        Tools,
        "List skill bundles",
        &[],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "reload-skills",
        &["reload_skills"],
        Tools,
        "Re-read the skill directories",
        &[],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "learn",
        &[],
        Tools,
        "Turn what this session did into a skill",
        &[text("focus")],
        CHATS,
        BusyPolicy::Reject,
        Effect::None,
    ),
    command(
        "refine",
        &[],
        Tools,
        "Review this conversation now for skills and memories worth keeping",
        &[text("focus")],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "memory",
        &[],
        Tools,
        "Show, edit or reload what the agent remembers",
        &[text("action")],
        CHATS,
        BusyPolicy::Reject,
        Effect::NextSession,
    ),
    command(
        "approvals",
        &[],
        Tools,
        "Show the approval mode and this session's approvals",
        &[],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "yolo",
        &[],
        Tools,
        "Allow every approval-gated call for this session (on|off)",
        &[arg("state", ArgKind::Choice(&["on", "off"]), false)],
        CHATS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "reload-secrets",
        &["reload_secrets"],
        Config,
        "Re-read the secrets store",
        &[],
        CHATS,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "config",
        &[],
        Config,
        "Show the config file, or get/set a key",
        &[text("get <key> | set <key> <value>")],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::Immediate,
    ),
    command(
        "theme",
        &[],
        Config,
        "List themes, or switch to one",
        &[arg("name", ArgKind::Word, false)],
        &[Surface::Cli, Surface::Tui],
        Immediate,
        Effect::Immediate,
    ),
    command(
        "verbose",
        &[],
        Config,
        "Cycle tool progress: off, new, all, verbose",
        &[],
        TERMINALS,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "profile",
        &[],
        Info,
        "Show the profile this session runs in",
        &[],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "image",
        &[],
        Session,
        "Attach an image to the next prompt; /image lists, /image clear drops",
        &[text("path|clear")],
        TERMINALS,
        Immediate,
        Effect::None,
    ),
    command(
        "prompt",
        &["compose", "edit"],
        Session,
        "Write the next message in your editor ($VISUAL, $EDITOR)",
        &[text("initial text")],
        TERMINALS,
        BusyPolicy::Reject,
        Effect::None,
    ),
    command(
        "queue",
        &["q"],
        Session,
        "Queue a prompt for after this turn; list, edit, rm, move, clear, add",
        &[text(
            "prompt | list | edit <n> <text> | rm <n> | move <a> <b> | clear | add <text>",
        )],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "history",
        &[],
        Info,
        "Print the last N messages of this session",
        &[arg("n", ArgKind::Word, false)],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "status",
        &[],
        Info,
        "Show this session's details and a recap (no model call)",
        &[],
        CHATS,
        Immediate,
        Effect::None,
    ),
    command(
        "version",
        &[],
        Info,
        "Show the version line",
        &[],
        EVERYWHERE,
        Immediate,
        Effect::None,
    ),
    command(
        "focus",
        &[],
        Config,
        "Hide tool progress for this session (on, off, status)",
        &[arg(
            "state",
            ArgKind::Choice(&["on", "off", "status"]),
            false,
        )],
        TERMINALS,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "timestamps",
        &["ts"],
        Config,
        "Show message timestamps (on, off, status); saved",
        &[arg(
            "state",
            ArgKind::Choice(&["on", "off", "status"]),
            false,
        )],
        TERMINALS,
        Immediate,
        Effect::Immediate,
    ),
    command(
        "exit",
        &["quit"],
        Exit,
        "Leave",
        &[],
        TERMINALS,
        InterruptThenDispatch,
        Effect::None,
    ),
];

#[cfg(test)]
mod tests {
    use super::super::{Registry, Surface};
    use super::BUILTINS;

    /// The build-time check of the spec: no two built-ins share a name or
    /// alias.
    #[test]
    fn built_in_names_and_aliases_are_unique() {
        Registry::with_builtins(BUILTINS).unwrap();
    }

    #[test]
    fn the_cli_set_of_m1_is_registered() {
        let registry = Registry::new().unwrap();
        for name in [
            "help",
            "new",
            "reset",
            "clear",
            "stop",
            "undo",
            "retry",
            "rewind",
            "fork",
            "resume",
            "sessions",
            "title",
            "model",
            "tools",
            "skills",
            "memory",
            "refine",
            "compress",
            "usage",
            "config",
            "theme",
            "approvals",
            "profile",
            "verbose",
            "exit",
            "quit",
        ] {
            let found = registry.builtin_names().contains(&name);
            assert!(found, "/{name} missing");
        }
        assert!(
            registry
                .builtin("stop")
                .is_some_and(|c| c.on(Surface::Gateway))
        );
    }
}
