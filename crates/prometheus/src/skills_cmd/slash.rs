//! `/skills <hub verb>` on every chat surface (spec: skills, Hub
//! discovery, inspection, updates and audit; Custom skill taps): the same
//! operations as the CLI, answered as text. Installs and removals apply
//! from the next session like every other skill change.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::text::Catalog;
use pmt_skillhub::install::{self, Options};
use pmt_skillhub::{Identifier, inspect, maintain, search, taps};
use pmt_skills::activation;
use pmt_skills::bundled::Bundled;

use super::Env;
use super::hub::{
    after_install, audit_text, check_text, hub, inspect_text, install_text, listing_text,
    update_text,
};

/// The verbs answered here (the rest go to `pmt_skills::activation`).
pub(crate) const VERBS: [&str; 11] = [
    "browse",
    "search",
    "inspect",
    "install",
    "uninstall",
    "check",
    "update",
    "audit",
    "accept",
    "tap",
    "untrust",
];

const USAGE: &str = "usage: /skills browse [--source S] | search <query> [--source S] | inspect <id> | install <id> [--force] [--name N] | uninstall <name> | check [name] | update [name] [--force] | audit | accept <name> | tap list|add <owner/repo> [--path P]|remove <owner/repo> | untrust <root>";

fn usage() -> Error {
    Error::new(&codes::CLI_USAGE, USAGE)
}

/// Words and `--flag [value]` options of the arguments.
struct Parsed<'a> {
    words: Vec<&'a str>,
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

fn parse(args: &str) -> Parsed<'_> {
    let mut parsed = Parsed {
        words: Vec::new(),
        flags: Vec::new(),
        values: Vec::new(),
    };
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--force" | "--locked" => parsed.flags.push(token),
            "--source" | "--name" | "--path" => {
                if let Some(value) = tokens.next() {
                    parsed.values.push((token, value));
                }
            }
            other => parsed.words.push(other),
        }
    }
    parsed
}

impl Parsed<'_> {
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| *v)
    }
}

/// Runs `/skills <args>` when its verb is a hub verb.
pub(crate) fn run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    args: &str,
) -> Option<Result<String, Error>> {
    let parsed = parse(args);
    let verb = *parsed.words.first()?;
    if !VERBS.contains(&verb) {
        return None;
    }
    let env = Env {
        scope: scope.clone(),
        user_home: launch.user_home().cloned(),
        cwd: None,
        bundled: Bundled::builtin(),
        json: false,
        launch: launch.clone(),
    };
    Some(dispatch(&env, verb, &parsed))
}

fn dispatch(env: &Env, verb: &str, p: &Parsed<'_>) -> Result<String, Error> {
    let rest = &p.words[1..];
    let strings = pmt_config::language::strings(env.scope.dir(), pmt_i18n::Surface::Core);
    let text: &dyn Catalog = &*strings;
    let deferred = || text.text(activation::DEFERRED, &[]);
    match (verb, rest) {
        ("browse", []) => Ok(listing_text(
            text,
            &search::browse(&hub(env)?, p.value("--source")),
        )),
        ("search", [_, ..]) => {
            let query = rest.join(" ");
            Ok(listing_text(
                text,
                &search::search(&hub(env)?, &query, p.value("--source")),
            ))
        }
        ("inspect", [id]) => Ok(inspect_text(
            text,
            &inspect::inspect(&hub(env)?, &Identifier::parse(id)?)?,
        )),
        ("install", [id]) => install(text, env, id, p),
        ("uninstall", [name]) => {
            maintain::uninstall(&hub(env)?, name)?;
            let done = text.text("slash.skills.uninstalled", &[("name", name)]);
            Ok(format!("{done} {}", deferred()))
        }
        ("check", [] | [_]) => Ok(check_text(
            text,
            &maintain::check(&hub(env)?, rest.first().copied())?,
        )),
        ("update", [] | [_]) => Ok(update_text(
            text,
            &maintain::update(&hub(env)?, rest.first().copied(), p.flag("--force"))?,
        )),
        ("audit", []) => Ok(audit_text(text, &maintain::audit(&hub(env)?)?)),
        ("accept", [name]) => {
            maintain::accept(&hub(env)?, name)?;
            let done = text.text("slash.skills.accepted", &[("name", name)]);
            Ok(format!("{done} {}", deferred()))
        }
        ("tap", _) => tap(text, env, rest, p),
        ("untrust", [root]) => activation::untrust(text, env.scope.profile(), Path::new(root)),
        _ => Err(usage()),
    }
}

fn install(text: &dyn Catalog, env: &Env, id: &str, p: &Parsed<'_>) -> Result<String, Error> {
    let options = Options {
        force: p.flag("--force"),
        name: p.value("--name").map(str::to_owned),
        ..Options::default()
    };
    let installed = install::install(&hub(env)?, &Identifier::parse(id)?, &options)?;
    after_install(env, &installed);
    Ok(install_text(text, &installed))
}

fn tap(text: &dyn Catalog, env: &Env, rest: &[&str], p: &Parsed<'_>) -> Result<String, Error> {
    let dir = env.scope.dir();
    match rest {
        ["list"] | [] => {
            let list = taps::read(dir)?;
            if list.is_empty() {
                return Ok(text.text("slash.skills.no_taps", &[]));
            }
            Ok(list
                .iter()
                .map(|t| text.text("slash.skills.tap", &[("repo", &t.repo), ("path", &t.path)]))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ["add", repo] => {
            taps::add(dir, repo, p.value("--path"))?;
            let trust = pmt_skillhub::sources::trust_text(hub(env)?.repo_trust(repo));
            Ok(text.text(
                "slash.skills.tap_added",
                &[("repo", repo), ("trust", &trust)],
            ))
        }
        ["remove", repo] => {
            taps::remove(dir, repo)?;
            Ok(text.text("slash.skills.tap_removed", &[("repo", repo)]))
        }
        _ => Err(usage()),
    }
}
