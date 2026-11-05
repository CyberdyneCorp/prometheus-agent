//! `prometheus bridge env <name>` (spec: python-bridge, Named side
//! environments): a pinned environment at
//! `<profile>/python/named/<name>-<lock_digest12>/` for skill scripts.
//! The lock is kept per requirement set, so a second run with the same
//! requirements resolves nothing and, once the environment exists, runs
//! no uv at all. Only the interpreter path goes to stdout.

use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use pmt_pybridge::env;
use sha2::{Digest as _, Sha256};

use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host::bridge::prepare::toolchain;

/// Whether `name` matches `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub(super) fn run(
    ctx: &Context<'_>,
    name: &str,
    files: &[PathBuf],
    extras: &[String],
) -> Result<u8, Error> {
    if !valid(name) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!("invalid environment name '{name}' (expected ^[a-z0-9][a-z0-9_-]{{0,63}}$)"),
        ));
    }
    let scope = ctx.scope()?;
    let mut requirements = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| Error::io(&codes::BRIDGE_REQUIREMENTS, file, &e))?;
        requirements.extend(env::parse_requirements(&text)?);
    }
    requirements.extend(extras.iter().cloned());
    env::check_bounds(
        &requirements,
        scope.config().python_bridge.allow_unbounded_requirements,
    )?;
    let (python, uv) = toolchain(&scope)?;
    let root = scope.dir().join("python").join("named");
    let wanted = pmt_pybridge::hex(&Sha256::digest(
        format!("{}\n{}", python.version, requirements.join("\n")).as_bytes(),
    ));
    let lock_dir = root
        .join(".locks")
        .join(format!("{name}-{}", &wanted[..12]));
    let lock = lock_dir.join(env::LOCK_FILE);
    if !lock.is_file() {
        eprintln!(
            "resolving {} requirement(s) for '{name}'",
            requirements.len()
        );
        env::lock(&uv, &python, &requirements, &lock_dir)?;
    }
    let built = env::ensure(&uv, &python, &root, name, &lock, false)?;
    print_raw(&format!("{}\n", built.python.display()))
}

#[cfg(test)]
mod tests {
    use super::valid;

    #[test]
    fn names_follow_the_pattern() {
        assert!(valid("ocr") && valid("pdf-tools_2"));
        assert!(!valid("Bad") && !valid("-x") && !valid(&"a".repeat(65)));
    }
}
