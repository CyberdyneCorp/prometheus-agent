//! What a task's `run.toml` may change (spec: replay-and-evaluation, Eval
//! run): settings that shape the agent's behaviour for the task, never the
//! isolation `eval run` puts its task sessions under. A suite is
//! third-party input (a directory under `<profile>/evals/`, a path, a
//! kanban card's suite), so its overrides are checked against an
//! allowlist: anything touching security, egress, the sandbox, the
//! terminal backend, memory, the run budget, provider endpoints or
//! credentials is refused, and `approvals.mode` may only name a mode that
//! still asks.

/// Settings a task may override, as dotted paths; `*` stands for one
/// segment and a path covers everything below it.
const ALLOWED: &[&str] = &[
    "agent.max_verify_nudges",
    "agent.reasoning_effort",
    "agent.tool_concurrency",
    "approvals.deny",
    "approvals.mode",
    "auxiliary.*.model",
    "auxiliary.*.provider",
    "auxiliary.*.timeout_secs",
    "budgets.session_tokens",
    "budgets.session_usd",
    "budgets.turn_tokens",
    "budgets.turn_usd",
    "compression",
    "context",
    "models.*.context_window",
    "models.*.max_output_tokens",
    "models.*.supports_reasoning",
    "models.*.supports_tools",
    "models.*.supports_video",
    "models.*.supports_vision",
    "tools",
];

/// Approval modes a task may pick: those that still ask (and so fail
/// closed in an unattended task session).
const ASKING_MODES: &[&str] = &["manual", "smart"];

/// Checks configuration `fragments` (TOML documents, or `key=value`
/// lines as for `--set`).
///
/// # Errors
///
/// The first fragment that does not parse, or the first key it sets that
/// a task may not override.
pub fn check(fragments: &[String]) -> Result<(), String> {
    for fragment in fragments {
        let table: toml::Table = fragment.parse().map_err(|e| format!("{fragment}: {e}"))?;
        check_table(&mut Vec::new(), &table)?;
    }
    Ok(())
}

fn check_table<'a>(path: &mut Vec<&'a str>, table: &'a toml::Table) -> Result<(), String> {
    for (key, value) in table {
        path.push(key);
        match value {
            toml::Value::Table(inner) => check_table(path, inner)?,
            leaf => check_leaf(path, leaf)?,
        }
        path.pop();
    }
    Ok(())
}

fn check_leaf(path: &[&str], value: &toml::Value) -> Result<(), String> {
    let dotted = path.join(".");
    if !ALLOWED.iter().any(|allowed| covers(allowed, path)) {
        return Err(format!(
            "'{dotted}' cannot be overridden by a task (it would change the run's isolation)"
        ));
    }
    if dotted == "approvals.mode" && !value.as_str().is_some_and(|m| ASKING_MODES.contains(&m)) {
        return Err(format!(
            "approvals.mode = {value} is refused: a task may only use a mode that asks ({})",
            ASKING_MODES.join(", ")
        ));
    }
    Ok(())
}

/// Whether `allowed` names `path` or a table above it.
fn covers(allowed: &str, path: &[&str]) -> bool {
    let pattern: Vec<&str> = allowed.split('.').collect();
    pattern.len() <= path.len()
        && pattern
            .iter()
            .zip(path)
            .all(|(want, got)| *want == "*" || want == got)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(fragment: &str) -> Result<(), String> {
        check(&[fragment.to_owned()])
    }

    #[test]
    fn behaviour_settings_pass() {
        assert!(checked("budgets.turn_tokens=600").is_ok());
        assert!(checked(r#"approvals.mode="smart""#).is_ok());
        assert!(checked(r#"auxiliary.approval.provider="mock""#).is_ok());
        assert!(
            checked("[models.\"mock/mock\"]\ncontext_window = 16000\n\n[compression]\ntarget_ratio = 0.1\n")
                .is_ok()
        );
    }

    #[test]
    fn isolation_settings_are_refused() {
        for fragment in [
            r#"approvals.mode="off""#,
            r#"approvals.single_query_mode="approve""#,
            "memory.memory_enabled=true",
            "[security.egress]\nmode = \"off\"\n",
            r#"terminal.backend="local""#,
            "budgets.root_usd=1000",
            r#"[models."x/y"]
base_url = "https://example.invalid""#,
            "approvals = { mode = \"off\" }",
        ] {
            assert!(checked(fragment).is_err(), "{fragment}");
        }
    }
}
