//! `cargo xtask health`: the complexity and unsafe-code ratchet
//! (spec: workspace-architecture, Complexity and unsafe-code ratchet).
//!
//! The limits live here, not in `health.toml`, so loosening them is a code
//! change rather than a config edit; `health.toml` only lists the modules
//! with a different policy and the recorded exceptions.

mod cognitive;
mod functions;
mod policy;
mod unsafe_scan;

use std::path::{Path, PathBuf};

use self::policy::{Measurement, Metric, Policy};

const COMPLEXITY_LIMIT: u32 = 15;
const PARSER_COMPLEXITY_LIMIT: u32 = 30;
const FUNCTION_LINES_LIMIT: u32 = 120;
const NESTING_LIMIT: u32 = 5;
const FILE_LINES_LIMIT: u32 = 1_500;

/// Directories, relative to the root, that hold the Rust sources.
const SOURCE_DIRS: [&str; 2] = ["crates", "xtask"];

/// Measures every source file under `root` against the budgets and
/// `root/health.toml`; with `base`, also rejects exceptions added or raised
/// relative to that older `health.toml`.
pub(crate) fn run(root: &Path, base: Option<&Path>) -> Result<(), String> {
    let policy = Policy::load(&root.join("health.toml"))?;
    let mut survey = Survey::default();
    for path in source_files(root)? {
        survey.add_file(root, &path, &policy)?;
    }
    let mut problems = survey.problems;
    problems.extend(policy::check_budgets(&policy, &survey.measurements));
    if let Some(base) = base {
        problems.extend(policy::check_ratchet(&policy, &Policy::load(base)?));
    }
    if problems.is_empty() {
        println!(
            "health: {} files and {} functions within budget",
            survey.files, survey.functions
        );
        return Ok(());
    }
    for problem in &problems {
        eprintln!("health: {problem}");
    }
    Err(format!(
        "{} health violation(s); see `health.toml` for the policy",
        problems.len()
    ))
}

#[derive(Default)]
struct Survey {
    files: usize,
    functions: usize,
    measurements: Vec<Measurement>,
    problems: Vec<String>,
}

impl Survey {
    fn add_file(&mut self, root: &Path, path: &Path, policy: &Policy) -> Result<(), String> {
        let rel = relative(root, path);
        let source = std::fs::read_to_string(path).map_err(|e| format!("{rel}: {e}"))?;
        let file = syn::parse_file(&source).map_err(|e| format!("{rel}: {e}"))?;
        self.files += 1;
        self.measure(
            &rel,
            Metric::FileLines,
            source.lines().count(),
            FILE_LINES_LIMIT,
        );
        let complexity_limit = if policy.is_parser_module(&rel) {
            PARSER_COMPLEXITY_LIMIT
        } else {
            COMPLEXITY_LIMIT
        };
        for function in functions::collect(&file) {
            self.functions += 1;
            let item = format!("{rel}::{}", function.name);
            self.measure(
                &item,
                Metric::Complexity,
                function.body.complexity,
                complexity_limit,
            );
            self.measure(&item, Metric::Lines, function.lines, FUNCTION_LINES_LIMIT);
            self.measure(&item, Metric::Nesting, function.body.nesting, NESTING_LIMIT);
        }
        self.check_unsafe(&rel, &source, policy)
    }

    fn measure(&mut self, item: &str, metric: Metric, value: impl TryInto<u32>, limit: u32) {
        self.measurements.push(Measurement {
            item: item.to_owned(),
            metric,
            value: value.try_into().unwrap_or(u32::MAX),
            limit,
        });
    }

    fn check_unsafe(&mut self, rel: &str, source: &str, policy: &Policy) -> Result<(), String> {
        for found in unsafe_scan::scan(source).map_err(|e| format!("{rel}: {e}"))? {
            let at = format!("{rel}:{}", found.line);
            if !policy.allows_unsafe(rel) {
                self.problems.push(format!(
                    "{at}: `unsafe` outside the modules allowlisted in health.toml (unsafe_modules)"
                ));
            }
            if found.needs_safety_comment && !found.has_safety_comment {
                self.problems
                    .push(format!("{at}: `unsafe` without a `// SAFETY:` comment"));
            }
        }
        Ok(())
    }
}

/// Every `.rs` file under the source directories, sorted so the report is
/// stable. Build output is skipped, and so are test fixtures, which break
/// the rules on purpose.
fn source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for dir in SOURCE_DIRS {
        let dir = root.join(dir);
        if dir.is_dir() {
            walk(&dir, &mut files).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    files.sort();
    Ok(files)
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if !is_skipped_dir(&path) {
                walk(&path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn is_skipped_dir(dir: &Path) -> bool {
    let name = dir.file_name().unwrap_or_default();
    let in_tests = dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|p| p == "tests");
    name == "target" || name.to_string_lossy().starts_with('.') || (in_tests && name == "fixtures")
}

/// `path` relative to `root`, with `/` separators on every host so that
/// `health.toml` entries are portable.
fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
