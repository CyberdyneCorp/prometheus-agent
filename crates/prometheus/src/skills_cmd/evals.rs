//! Running skill evals (spec: skills, Skill evals; Eval-gated skill
//! edits). Each run of a case gets a temporary home of its own: one
//! profile holding only the skill under test (its `evals/` left out, so
//! the model never sees the expectations), a workspace with the case's
//! `setup_files`, and a one-shot `chat -z` of this binary in it, so the
//! case runs isolated in the child's own sandbox. The child's store gives
//! the tool calls; its stdout is the answer. With `--provider mock` the
//! child plays the case's `mock_script` (deterministic and offline); with
//! `live` it uses a copy of the profile's configuration and its secrets
//! (passed as `PMT_SECRET_<NAME>`, never on argv). A `judge` rubric goes
//! to the profile's `auxiliary.eval_judge` route.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_skills::evals::{self, Case, Evals, Gate, Observed, Rate};
use pmt_store::{ListOptions, NewSession, Source, Store, Visibility};
use serde::Serialize;
use serde_json::Value;

use crate::chat::session::model::{Models, RouteFlags};

/// Which model a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    /// The mock provider playing each case's script.
    Mock,
    /// The profile's configured model.
    Live,
}

/// One case's runs.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CaseReport {
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) runs: u32,
    pub(crate) passed_runs: u32,
    /// Why each failed run failed (`run n: reason; reason`).
    pub(crate) failures: Vec<String>,
}

/// One skill's results.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillReport {
    pub(crate) skill: String,
    pub(crate) cases: Vec<CaseReport>,
    pub(crate) rate: Rate,
}

impl SkillReport {
    pub(crate) fn passed(&self) -> bool {
        self.cases.iter().all(|c| c.passed)
    }
}

/// What runs a skill's cases.
pub(crate) struct Runner {
    scope: ProfileScope,
    store: Arc<Store>,
    launch: LaunchEnv,
    provider: Provider,
    repeat: u32,
    /// Where temporary homes go (removed after each run).
    work: PathBuf,
}

impl Runner {
    pub(crate) fn new(
        scope: &ProfileScope,
        store: &Arc<Store>,
        launch: &LaunchEnv,
        provider: Provider,
        repeat: u32,
    ) -> Self {
        let work = scope
            .path("evals/skills/.work")
            .join(ulid::Ulid::generate().to_string().to_lowercase());
        Self {
            scope: scope.clone(),
            store: Arc::clone(store),
            launch: launch.clone(),
            provider,
            repeat: repeat.max(1),
            work,
        }
    }

    /// Runs every case of the skill whose files are `files` (as
    /// `pmt_skills::catalog::files_of` lists them); `Ok(None)` when it
    /// has no evals.
    pub(crate) fn skill(
        &self,
        name: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<Option<SkillReport>, Error> {
        let source = self.work.join(format!("skill-{}", ulid::Ulid::generate()));
        write_files(
            &source,
            files.iter().map(|(rel, bytes)| (rel.as_str(), bytes)),
        )?;
        let loaded = Evals::load(&source).map_err(|e| Error::new(&codes::SKILL_INVALID, e));
        let report = match loaded {
            Ok(Some(evals)) => Some(self.cases(name, files, &evals)),
            Ok(None) => None,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&source);
                return Err(e);
            }
        };
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir(&self.work);
        Ok(report)
    }

    fn cases(&self, name: &str, files: &[(String, Vec<u8>)], evals: &Evals) -> SkillReport {
        let mut cases = Vec::new();
        let mut rate = Rate::default();
        for case in &evals.cases {
            let mut report = CaseReport {
                name: case.name.clone(),
                passed: true,
                runs: self.repeat,
                passed_runs: 0,
                failures: Vec::new(),
            };
            for n in 1..=self.repeat {
                let failed = match self.run(name, files, evals, case) {
                    Ok(seen) => evals::failures(name, case, &seen),
                    Err(why) => vec![format!("could not run: {why}")],
                };
                if failed.is_empty() {
                    report.passed_runs += 1;
                } else {
                    report.passed = false;
                    report
                        .failures
                        .push(format!("run {n}: {}", failed.join("; ")));
                }
            }
            rate.passed += report.passed_runs;
            rate.total += report.runs;
            cases.push(report);
        }
        SkillReport {
            skill: name.to_owned(),
            cases,
            rate,
        }
    }

    /// One run of `case` in a temporary home.
    fn run(
        &self,
        name: &str,
        files: &[(String, Vec<u8>)],
        evals: &Evals,
        case: &Case,
    ) -> Result<Observed, String> {
        let scratch = self
            .work
            .join(ulid::Ulid::generate().to_string().to_lowercase());
        let result = self.run_in(&scratch, name, files, evals, case);
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }

    fn run_in(
        &self,
        scratch: &Path,
        name: &str,
        files: &[(String, Vec<u8>)],
        evals: &Evals,
        case: &Case,
    ) -> Result<Observed, String> {
        let home = scratch.join("home");
        let profile = home.join("profiles/default");
        let work = scratch.join("work");
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        // The skill without its evals: the model never sees expectations.
        let skill = files
            .iter()
            .filter(|(rel, _)| !rel.starts_with("evals/"))
            .map(|(rel, bytes)| (rel.as_str(), bytes));
        write_files(&profile.join("skills").join(name), skill).map_err(|e| e.to_string())?;
        std::fs::write(profile.join("config.toml"), self.config()?).map_err(|e| e.to_string())?;
        for rel in &case.setup_files {
            let to = work.join(rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(evals.dir.join(rel), &to).map_err(|e| format!("{rel}: {e}"))?;
        }
        let output = self.chat(&home, &work, evals, case)?;
        let mut seen = Observed {
            output: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            calls: calls(&home)?,
            judged: None,
        };
        if let Some(rubric) = &case.judge {
            seen.judged = Some(self.judge(rubric, case, &seen.output)?);
        }
        Ok(seen)
    }

    /// The temporary profile's configuration.
    fn config(&self) -> Result<String, String> {
        match self.provider {
            Provider::Mock => Ok("[model]\nprovider = \"mock\"\ndefault = \"mock\"\n".to_owned()),
            Provider::Live => {
                let path = self.scope.path("config.toml");
                match std::fs::read_to_string(&path) {
                    Ok(text) => Ok(text),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                    Err(e) => Err(format!("{}: {e}", path.display())),
                }
            }
        }
    }

    fn chat(
        &self,
        home: &Path,
        work: &Path,
        evals: &Evals,
        case: &Case,
    ) -> Result<std::process::Output, String> {
        let bin = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = crate::smoke_suite::runner::base(&bin, home);
        cmd.args(["chat", "-z", &case.prompt]);
        match self.provider {
            Provider::Mock => {
                cmd.args(["--provider", "mock"]);
                if let Some(script) = &case.mock_script {
                    cmd.arg("--mock-script").arg(evals.dir.join(script));
                }
            }
            Provider::Live => {
                let secrets = self.scope.secrets().store();
                for secret in secrets.names().map_err(|e| e.message().to_owned())? {
                    if let Ok(Some(value)) = secrets.get(&secret) {
                        cmd.env(format!("PMT_SECRET_{secret}"), value.expose());
                    }
                }
            }
        }
        cmd.current_dir(work)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())
    }

    fn judge(&self, rubric: &str, case: &Case, answer: &str) -> Result<(bool, String), String> {
        let models = Models::open(
            &self.scope,
            &self.store,
            self.launch.user_home().map(PathBuf::as_path),
            &RouteFlags::default(),
            None,
        )
        .map_err(|e| e.message().to_owned())?;
        let session = self
            .store
            .create_session(&NewSession {
                source_tag: Some("skill-eval".to_owned()),
                hidden: true,
                ..NewSession::new(Source::Cli)
            })
            .map_err(|e| e.message().to_owned())?;
        let user = format!(
            "Rubric: {rubric}\n\nTask given to the assistant: {}\n\nThe assistant's answer:\n{answer}",
            case.prompt
        );
        let reply = crate::model_cmd::block_on(models.ask_auxiliary(
            pmt_providers::auxiliary::AuxTask::EvalJudge,
            session.id,
            evals::JUDGE_SYSTEM,
            &user,
            200,
        ))
        .map_err(|e| e.message().to_owned())?
        .map_err(|e| e.message().to_owned())?;
        Ok(reply.map_or_else(
            || (false, "the judge did not answer".to_owned()),
            |r| evals::verdict(&r),
        ))
    }
}

fn write_files<'a>(
    dir: &Path,
    files: impl Iterator<Item = (&'a str, &'a Vec<u8>)>,
) -> Result<(), Error> {
    let io = |path: &Path, e: std::io::Error| {
        Error::new(&codes::SKILL_IO, format!("{}: {e}", path.display()))
    };
    for (rel, bytes) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| io(&path, e))?;
    }
    Ok(())
}

/// Every tool call recorded in the temporary home's store.
fn calls(home: &Path) -> Result<Vec<(String, Value)>, String> {
    let read = || -> Result<Vec<(String, Value)>, Error> {
        let home = Home::open(home)?;
        let dir = home.profile(&ProfileName::parse("default")?)?;
        let scope = ProfileScope::open(dir, &Overrides::parse::<&str>(&[])?, Launch::Other)?;
        let store = Store::open(&scope)?;
        let sessions = store.list_sessions(&ListOptions {
            all: true,
            ..ListOptions::default()
        })?;
        let mut out = Vec::new();
        for session in sessions {
            for message in store.messages(session.id, Visibility::All)? {
                let listed = message.tool_calls.as_ref().and_then(Value::as_array);
                for call in listed.into_iter().flatten() {
                    let name = call["name"].as_str().unwrap_or_default().to_owned();
                    let args = match &call["arguments"] {
                        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
                        other => other.clone(),
                    };
                    out.push((name, args));
                }
            }
        }
        Ok(out)
    };
    read().map_err(|e| e.message().to_owned())
}

/// Runs `name`'s evals the way an eval-gated edit needs: the default
/// provider, `repeat` runs per case.
pub(crate) struct ProfileGate {
    pub(crate) scope: ProfileScope,
    pub(crate) store: Arc<Store>,
    pub(crate) launch: LaunchEnv,
}

impl Gate for ProfileGate {
    fn rate(&self, skill_dir: &Path, name: &str, repeat: u32) -> Result<Option<Rate>, String> {
        if !evals::has_evals(skill_dir) {
            return Ok(None);
        }
        let files: Vec<(String, Vec<u8>)> = pmt_skills::layout::walk(skill_dir)
            .into_iter()
            .filter_map(|rel| std::fs::read(skill_dir.join(&rel)).ok().map(|b| (rel, b)))
            .collect();
        let runner = Runner::new(
            &self.scope,
            &self.store,
            &self.launch,
            Provider::Mock,
            repeat,
        );
        runner
            .skill(name, &files)
            .map(|report| report.map(|r| r.rate))
            .map_err(|e| e.message().to_owned())
    }
}
