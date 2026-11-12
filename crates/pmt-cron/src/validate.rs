//! Payload validation (spec: scheduled-jobs, Job payload validation): a
//! job is checked whole at create and update, before anything is written.

use std::path::{Component, Path, PathBuf};

use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};

use crate::job::JobSpec;
use crate::target::Target;

/// The longest explicit grace.
pub const MAX_GRACE_SECS: u64 = 86_400;

/// The profile's registries, as validation asks about them.
pub trait Catalog {
    /// Whether skill `name` is installed.
    fn skill_exists(&self, name: &str) -> bool;
    /// Whether toolset `name` is registered.
    fn toolset_exists(&self, name: &str) -> bool;
    /// Whether the pinned provider and model are in the provider registry
    /// (`Err` names what is not).
    ///
    /// # Errors
    ///
    /// The unknown provider or model.
    fn check_model(&self, provider: Option<&str>, model: Option<&str>) -> Result<(), String>;
    /// Whether profile `name` exists under the same Prometheus home.
    fn profile_exists(&self, name: &str) -> bool;
    /// Whether secret `name` is a provider credential or gateway-owned
    /// (never passed to scripts).
    fn secret_reserved(&self, name: &str) -> bool;
}

/// The reasoning levels a job may pin.
pub const REASONING_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::CRON_INVALID_JOB, message)
}

/// Whether `script`, joined to a directory, names something inside it
/// lexically: relative, with no `..`, root or prefix component. Checked
/// before any path is built from a script name.
#[must_use]
pub fn stays_inside(script: &str) -> bool {
    let relative = Path::new(script);
    !relative.is_absolute()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// The script file of `script` inside `scripts_dir`.
///
/// # Errors
///
/// `cron.invalid_job` when it resolves outside the directory (by `..`,
/// an absolute path or a symbolic link) or does not exist.
pub fn script_path(scripts_dir: &Path, script: &str) -> Result<PathBuf, Error> {
    let outside = || {
        invalid(format!(
            "script path resolves outside {}",
            scripts_dir.display()
        ))
    };
    if !stays_inside(script) {
        return Err(outside());
    }
    let path = scripts_dir.join(script);
    let resolved = path.canonicalize().map_err(|_| {
        invalid(format!(
            "script '{script}' not found in {}",
            scripts_dir.display()
        ))
    })?;
    let root = scripts_dir.canonicalize().map_err(|_| outside())?;
    if !resolved.starts_with(&root) {
        return Err(outside());
    }
    Ok(resolved)
}

/// Checks `spec` for the profile whose scripts live in `scripts_dir`.
///
/// # Errors
///
/// `cron.invalid_job` naming the first problem.
pub fn validate(spec: &JobSpec, scripts_dir: &Path, catalog: &dyn Catalog) -> Result<(), Error> {
    if spec.prompt.trim().is_empty() && spec.skills.is_empty() && spec.script.is_none() {
        return Err(invalid(
            "cron job has nothing to run: provide a prompt, a script, or at least one skill",
        ));
    }
    if spec.no_agent && spec.script.is_none() {
        return Err(invalid(
            "no_agent needs a script: the script is the whole job",
        ));
    }
    if let Some(script) = &spec.script {
        script_path(scripts_dir, script)?;
    }
    if let Some(skill) = spec.skills.iter().find(|s| !catalog.skill_exists(s)) {
        return Err(invalid(format!("unknown skill '{skill}'")));
    }
    if let Some(toolsets) = &spec.toolsets
        && let Some(toolset) = toolsets.iter().find(|t| !catalog.toolset_exists(t))
    {
        return Err(invalid(format!("unknown toolset '{toolset}'")));
    }
    for target in spec.deliver.iter().chain(&spec.failure_deliver) {
        if let Target::BotChat(Some(profile)) = Target::parse(target)?
            && !catalog.profile_exists(&profile)
        {
            return Err(invalid(format!("unknown profile '{profile}'")));
        }
    }
    check_workdir(spec)?;
    check_script_fields(spec, scripts_dir, catalog)?;
    if spec.model.is_some() || spec.provider.is_some() {
        catalog
            .check_model(spec.provider.as_deref(), spec.model.as_deref())
            .map_err(invalid)?;
    }
    for (i, entry) in spec.fallback_chain.iter().flatten().enumerate() {
        catalog
            .check_model(Some(&entry.provider), Some(&entry.model))
            .map_err(|why| invalid(format!("fallback_chain[{i}]: {why}")))?;
    }
    check_limits(spec)
}

/// `workdir` is an absolute path to an existing directory.
///
/// # Errors
///
/// `cron.invalid_workdir` (exit 2) naming the path.
pub fn check_workdir(spec: &JobSpec) -> Result<(), Error> {
    match spec.workdir.as_deref() {
        Some(dir) if !(Path::new(dir).is_absolute() && Path::new(dir).is_dir()) => Err(Error::new(
            &codes::CRON_INVALID_WORKDIR,
            format!("workdir must be an existing absolute directory: {dir}"),
        )),
        _ => Ok(()),
    }
}

fn check_script_fields(
    spec: &JobSpec,
    scripts_dir: &Path,
    catalog: &dyn Catalog,
) -> Result<(), Error> {
    if let Some(name) = spec
        .script_secrets
        .iter()
        .find(|s| catalog.secret_reserved(s))
    {
        return Err(invalid(format!(
            "secret '{name}' cannot be passed to cron scripts"
        )));
    }
    if spec
        .interpreter
        .as_deref()
        .is_some_and(|i| !Path::new(i).is_absolute())
    {
        return Err(invalid("interpreter must be an absolute path"));
    }
    if spec.monitor_script.is_some() && spec.monitor_url.is_some() {
        return Err(invalid("set monitor_script or monitor_url, not both"));
    }
    if spec.no_agent && (spec.monitor_script.is_some() || spec.monitor_url.is_some()) {
        return Err(invalid(
            "monitor mode needs the agent: it cannot be no_agent",
        ));
    }
    if let Some(script) = &spec.monitor_script {
        script_path(scripts_dir, script)?;
    }
    if let Some(url) = &spec.monitor_url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(invalid(format!(
            "monitor_url must be an http or https URL: {url}"
        )));
    }
    if spec
        .reasoning_effort
        .as_deref()
        .is_some_and(|r| !REASONING_LEVELS.contains(&r))
    {
        return Err(invalid(format!(
            "unknown reasoning effort (expected {})",
            REASONING_LEVELS.join(", ")
        )));
    }
    Ok(())
}

fn check_limits(spec: &JobSpec) -> Result<(), Error> {
    if spec.grace_secs.is_some_and(|g| g > MAX_GRACE_SECS) {
        return Err(invalid("grace must be between 0s and 24h"));
    }
    if spec.repeat_times == Some(0) {
        return Err(invalid("repeat must be at least 1"));
    }
    if let Some(zone) = &spec.timezone
        && TimeZone::get(zone).is_err()
    {
        return Err(invalid(format!("unknown time zone '{zone}'")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Catalog, validate};
    use crate::job::JobSpec;
    use crate::schedule::Schedule;

    struct Known;

    impl Catalog for Known {
        fn skill_exists(&self, name: &str) -> bool {
            name == "digest"
        }
        fn toolset_exists(&self, name: &str) -> bool {
            ["web", "file", "core"].contains(&name)
        }
        fn check_model(&self, _: Option<&str>, model: Option<&str>) -> Result<(), String> {
            match model {
                Some("nope") => Err("unknown model 'nope'".into()),
                _ => Ok(()),
            }
        }
        fn profile_exists(&self, name: &str) -> bool {
            name == "assistant"
        }
        fn secret_reserved(&self, name: &str) -> bool {
            name == "OPENAI_API_KEY"
        }
    }

    fn spec() -> JobSpec {
        JobSpec::new(Schedule::Interval { every_secs: 3600 }, "summarise")
    }

    fn check(spec: &JobSpec) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts/ok.sh"), "echo ok").unwrap();
        std::fs::write(dir.path().join("secret.sh"), "x").unwrap();
        validate(spec, &dir.path().join("scripts"), &Known).map_err(|e| e.message().to_owned())
    }

    /// Scenario: Unknown toolset is an error.
    #[test]
    fn an_unknown_toolset_is_refused() {
        let mut job = spec();
        job.toolsets = Some(vec!["web".into(), "wbe".into()]);
        assert_eq!(check(&job).unwrap_err(), "unknown toolset 'wbe'");
    }

    #[test]
    fn payload_rules() {
        assert!(check(&spec()).is_ok());
        let mut job = spec();
        job.prompt = "  ".into();
        assert_eq!(
            check(&job).unwrap_err(),
            "cron job has nothing to run: provide a prompt, a script, or at least one skill"
        );
        job.script = Some("ok.sh".into());
        assert!(check(&job).is_ok());
        job.script = Some("../secret.sh".into());
        assert!(
            check(&job)
                .unwrap_err()
                .starts_with("script path resolves outside")
        );
        let mut job = spec();
        job.no_agent = true;
        assert!(
            check(&job)
                .unwrap_err()
                .starts_with("no_agent needs a script")
        );
        let mut job = spec();
        job.skills = vec!["nosuch".into()];
        assert_eq!(check(&job).unwrap_err(), "unknown skill 'nosuch'");
        let mut job = spec();
        job.deliver = vec!["tele gram".into()];
        assert!(
            check(&job)
                .unwrap_err()
                .starts_with("unknown delivery target")
        );
        let mut job = spec();
        job.model = Some("nope".into());
        assert_eq!(check(&job).unwrap_err(), "unknown model 'nope'");
        let mut job = spec();
        job.grace_secs = Some(86_401);
        assert!(check(&job).is_err());
        job.grace_secs = Some(0);
        assert!(check(&job).is_ok());
    }

    /// Scenario: Unknown profile rejected at create.
    #[test]
    fn an_unknown_bot_chat_profile_is_refused() {
        let mut job = spec();
        job.deliver = vec!["bot_chat:nobody".into()];
        assert_eq!(check(&job).unwrap_err(), "unknown profile 'nobody'");
        job.deliver = vec!["bot_chat:assistant".into(), "bot_chat".into()];
        assert!(check(&job).is_ok());
    }

    /// Scenario: Provider key cannot reach a script.
    #[test]
    fn a_provider_key_cannot_reach_a_script() {
        let mut job = spec();
        job.script_secrets = vec!["OPENAI_API_KEY".into()];
        assert_eq!(
            check(&job).unwrap_err(),
            "secret 'OPENAI_API_KEY' cannot be passed to cron scripts"
        );
    }

    #[test]
    fn workdir_and_monitor_rules() {
        let mut job = spec();
        job.workdir = Some("../acme".into());
        assert_eq!(
            check(&job).unwrap_err(),
            "workdir must be an existing absolute directory: ../acme"
        );
        job.workdir = Some(std::env::temp_dir().display().to_string());
        assert!(check(&job).is_ok());
        let mut job = spec();
        job.monitor_url = Some("ftp://x".into());
        assert!(check(&job).unwrap_err().starts_with("monitor_url must be"));
        job.monitor_url = Some("https://example.com".into());
        job.monitor_script = Some("ok.sh".into());
        assert!(check(&job).unwrap_err().contains("not both"));
    }
}
