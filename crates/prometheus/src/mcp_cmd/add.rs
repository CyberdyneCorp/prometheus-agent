//! `prometheus mcp add` (spec: plugin-system, mcp CLI).
//!
//! Writes `[mcp.servers.<name>]` from the flags over a transport preset
//! (`codex`: `command = "codex"`, `args = ["mcp-server"]`; explicit flags
//! win), then probes the server: on a terminal its tool checklist is
//! offered (`tools.include` is written only when not every tool is
//! selected). A server that does not answer stays configured.

use clap::ValueEnum;
use pmt_core::error::{Error, codes};

use super::{AddArgs, Trust, exists, json};
use crate::context::Context;
use crate::setup_cmd::Wizard;

/// `--preset`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Preset {
    /// OpenAI Codex as an MCP server: `codex mcp-server`.
    Codex,
}

impl Preset {
    /// The preset's command and arguments.
    fn launch(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Codex => ("codex", &["mcp-server"]),
        }
    }
}

/// `--auth`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Auth {
    /// The `--auth-secret` bearer token.
    Header,
    /// OAuth 2.1 (sign in with `prometheus mcp login <name>`).
    Oauth,
}

pub(super) fn add(ctx: &Context<'_>, args: &AddArgs) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    if !pmt_config::mcp_check::is_server_name(&args.name) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "invalid MCP server name '{}': use 1-64 letters, digits, '_' or '-'",
                args.name
            ),
        ));
    }
    if scope.config().mcp.servers.contains_key(&args.name) {
        return Err(exists(&args.name));
    }
    let pairs = pairs(args)?;
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    scope.profile().config_file().set_all(&borrowed)?;
    println!(
        "added MCP server '{}'; its tools join sessions started from now",
        args.name
    );
    if matches!(args.auth, Some(Auth::Oauth)) {
        println!("sign in first: prometheus mcp login {}", args.name);
        return Ok(0);
    }
    let mut wizard = Wizard::stdio();
    super::tools::probe_and_choose(ctx, &args.name, &mut wizard)?;
    Ok(0)
}

/// The `config.toml` keys `mcp add` writes.
fn pairs(args: &AddArgs) -> Result<Vec<(String, String)>, Error> {
    let preset = args.preset.map(Preset::launch);
    let command = args
        .command
        .clone()
        .or_else(|| preset.map(|(c, _)| c.to_owned()));
    let mut pairs = match (&args.url, command) {
        (Some(url), _) => http_pairs(args, url),
        (None, Some(command)) => stdio_pairs(args, &command, preset.map(|(_, a)| a))?,
        (None, None) => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "give --command, --url or --preset",
            ));
        }
    };
    if let Some(trust) = args.trust {
        let value = match trust {
            Trust::Full => "\"full\"",
            Trust::Untrusted => "\"untrusted\"",
        };
        pairs.push((key(args, "trust"), value.to_owned()));
    }
    Ok(pairs)
}

fn key(args: &AddArgs, field: &str) -> String {
    format!("mcp.servers.{}.{field}", args.name)
}

fn http_pairs(args: &AddArgs, url: &str) -> Vec<(String, String)> {
    let mut pairs = vec![
        (key(args, "transport"), "\"http\"".to_owned()),
        (key(args, "url"), json(&url)),
    ];
    if let Some(secret) = &args.auth_secret {
        pairs.push((key(args, "auth_secret"), json(secret)));
    }
    match args.auth {
        Some(Auth::Oauth) => pairs.push((key(args, "auth"), "\"oauth\"".to_owned())),
        Some(Auth::Header) => pairs.push((key(args, "auth"), "\"header\"".to_owned())),
        None => {}
    }
    pairs
}

fn stdio_pairs(
    args: &AddArgs,
    command: &str,
    preset_args: Option<&[&str]>,
) -> Result<Vec<(String, String)>, Error> {
    let mut pairs = vec![
        (key(args, "transport"), "\"stdio\"".to_owned()),
        (key(args, "command"), json(&command)),
    ];
    let words: Vec<String> = if args.args.is_empty() {
        preset_args
            .map(|a| a.iter().map(|&w| w.to_owned()).collect())
            .unwrap_or_default()
    } else {
        args.args.clone()
    };
    if !words.is_empty() {
        pairs.push((key(args, "args"), json(&words)));
    }
    for pair in &args.env {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("--env {pair}: expected KEY=VALUE"),
            ));
        };
        pairs.push((key(args, &format!("env.{name}")), json(&value)));
    }
    if !args.env_secrets.is_empty() {
        pairs.push((key(args, "env_secrets"), json(&args.env_secrets)));
    }
    Ok(pairs)
}
