//! Harness validation contracts (spec: harness-evolution, Harness tree and
//! on-disk layout; Editable surface and component numbering; Harness size
//! caps; Harness validation; Harness execution confinement).

use pmt_config::harness::Tree;
use pmt_core::secret::{Redactor, SecretString};
use pmt_evolve::harness::{Rules, validate};

fn rules() -> Rules {
    Rules::with_toolsets(["file".to_owned(), "web".to_owned()])
}

fn tree(files: &[(&str, &str)]) -> Tree {
    let mut tree = Tree::builtin();
    for (path, text) in files {
        tree.files.insert((*path).to_owned(), (*text).to_owned());
    }
    tree
}

fn errors(files: &[(&str, &str)]) -> Vec<String> {
    validate(&mut tree(files), &rules()).expect_err("the tree must fail")
}

fn has(errors: &[String], needle: &str) -> bool {
    errors.iter().any(|e| e.contains(needle))
}

/// Scenario: Traversal rejected.
#[test]
fn a_traversal_key_fails_naming_the_path() {
    let errors = errors(&[("../config.toml", "x")]);
    assert!(has(&errors, "invalid path '../config.toml'"), "{errors:?}");
}

/// Scenario: Off-surface write.
#[test]
fn off_surface_paths_are_listed_and_reject_the_tree() {
    let errors = errors(&[("memories/MEMORY.md", "x"), ("scripts/Clean.py", "x = 1\n")]);
    assert!(
        errors.contains(&"off-surface paths: memories/MEMORY.md, scripts/Clean.py".to_owned()),
        "{errors:?}"
    );
    let many: Vec<(String, String)> = (1..=8)
        .map(|i| (format!("notes/n{i}.md"), String::new()))
        .collect();
    let many: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let errors = self::errors(&many);
    assert!(has(&errors, "(+2 more)"), "{errors:?}");
}

/// Scenario: Always-on cap exceeded.
#[test]
fn always_on_text_one_char_over_the_cap_fails() {
    let system = "a".repeat(12_000);
    let guardrails = "b".repeat(12_001);
    let errors = errors(&[("SYSTEM.md", &system), ("GUARDRAILS.md", &guardrails)]);
    assert!(
        errors.contains(&"always-on text is 24001 chars; cap is 24000".to_owned()),
        "{errors:?}"
    );
    let ok = "c".repeat(12_000);
    assert!(
        validate(
            &mut tree(&[("SYSTEM.md", &system), ("GUARDRAILS.md", &ok)]),
            &rules()
        )
        .is_ok()
    );
}

/// Scenario: Missing referenced script.
#[test]
fn a_referenced_script_must_exist() {
    let errors = errors(&[("SYSTEM.md", "Before answering, run scripts/clean.py.")]);
    assert!(has(&errors, "scripts/clean.py"), "{errors:?}");
    let ok = &mut tree(&[
        ("SYSTEM.md", "Before answering, run scripts/clean.py."),
        ("scripts/clean.py", "print('ok')\n"),
    ]);
    assert!(validate(ok, &rules()).is_ok());
}

/// Scenario: Skill without trigger line is repaired.
#[test]
fn an_unnamed_skill_gets_its_trigger_line() {
    let mut tree = tree(&[(
        "skills/recon/SKILL.md",
        "---\nname: recon\ndescription: reconciling ledgers\n---\nSum the debits.\n",
    )]);
    let validated = validate(&mut tree, &rules()).expect("valid after repair");
    assert_eq!(validated.repaired, ["recon"]);
    assert!(
        tree.get("GUARDRAILS.md")
            .unwrap()
            .contains("- When reconciling ledgers, load the `recon` skill."),
        "{tree:?}"
    );
    assert_eq!(validated.census.skills, 1);
    // A skill already named is left alone.
    let mut named = self::tree(&[
        ("SYSTEM.md", "Use the recon skill for ledgers."),
        (
            "skills/recon/SKILL.md",
            "---\nname: recon\ndescription: reconciling ledgers\n---\n",
        ),
    ]);
    assert!(validate(&mut named, &rules()).unwrap().repaired.is_empty());
}

#[test]
fn a_trigger_line_over_the_cap_is_skipped_and_the_skill_is_an_error() {
    let full = "a".repeat(24_000);
    let errors = errors(&[
        ("SYSTEM.md", &full),
        (
            "skills/recon/SKILL.md",
            "---\nname: recon\ndescription: reconciling ledgers\n---\n",
        ),
    ]);
    assert!(has(&errors, "skill 'recon' is not named"), "{errors:?}");
    assert!(!has(&errors, "always-on text is"), "{errors:?}");
}

/// Scenario: Unknown hook event.
#[test]
fn an_unknown_hook_event_is_refused() {
    let errors = errors(&[
        (
            "hooks.toml",
            "[[hooks]]\nevent = \"on_magic\"\ncommand = \"python3 scripts/h.py\"\n",
        ),
        ("scripts/h.py", "print(1)\n"),
    ]);
    assert!(
        errors.contains(&"unknown hook event 'on_magic'".to_owned()),
        "{errors:?}"
    );
}

/// Scenario: Hook asking for network.
#[test]
fn a_hooks_file_cannot_ask_for_network() {
    let errors = errors(&[("hooks.toml", "network = [\"api.example.com\"]\n")]);
    assert!(
        errors.contains(&"unknown key 'network' in hooks.toml".to_owned()),
        "{errors:?}"
    );
    let errors = self::errors(&[
        (
            "hooks.toml",
            "[[hooks]]\nevent = \"pre_tool_call\"\ncommand = \"python3 scripts/h.py\"\ncapabilities = [\"net\"]\ntimeout_secs = 500\n",
        ),
        ("scripts/h.py", "print(1)\n"),
    ]);
    assert!(
        has(&errors, "unknown key 'capabilities' in hooks.toml"),
        "{errors:?}"
    );
    assert!(
        has(&errors, "timeout_secs must be from 1 to 120"),
        "{errors:?}"
    );
}

#[test]
fn hook_commands_may_run_only_harness_scripts() {
    let errors = errors(&[(
        "hooks.toml",
        "[[hooks]]\nevent = \"pre_tool_call\"\ncommand = \"curl https://example.com/x.sh\"\n",
    )]);
    assert!(has(&errors, "must run a harness script"), "{errors:?}");
    let mut ok = tree(&[
        (
            "hooks.toml",
            "[[hooks]]\nevent = \"pre_tool_call\"\ncommand = \"sh scripts/guard.sh\"\nmatcher = \"^terminal$\"\n",
        ),
        ("scripts/guard.sh", "#!/bin/sh\nexit 0\n"),
    ]);
    assert_eq!(validate(&mut ok, &rules()).unwrap().census.hooks, 1);
}

#[test]
fn scripts_are_parsed_without_running() {
    let errors = errors(&[
        ("scripts/bad.py", "def broken(:\n"),
        ("scripts/bad.sh", "if then fi (\n"),
    ]);
    assert!(has(&errors, "scripts/bad.py: line 1"), "{errors:?}");
    assert!(has(&errors, "scripts/bad.sh: line"), "{errors:?}");
}

#[test]
fn every_error_is_collected_in_one_pass() {
    let errors = errors(&[
        ("SYSTEM.md", "Hello {{ name }}"),
        ("config.toml", "x"),
        (
            "agents/critic/AGENT.md",
            "---\nname: critic\ndescription: Reviews\ntoolsets: [shell_magic]\n---\n",
        ),
        (
            "skills/web/SKILL.md",
            "---\nname: other\ndescription: d\n---\n",
        ),
    ]);
    assert!(
        has(&errors, "SYSTEM.md contains template syntax"),
        "{errors:?}"
    );
    assert!(has(&errors, "off-surface paths: config.toml"), "{errors:?}");
    assert!(has(&errors, "unknown toolset 'shell_magic'"), "{errors:?}");
    assert!(
        has(&errors, "does not match its directory 'web'"),
        "{errors:?}"
    );
}

#[test]
fn secrets_and_installed_skill_names_are_refused() {
    let mut rules = rules();
    rules.installed_skills.insert("recon".to_owned());
    let secret = SecretString::new("hunter2-very-secret-value");
    rules.redactor = Redactor::named([("TOKEN", &secret)]);
    let mut tree = tree(&[
        ("TOOL_GUIDANCE.md", "the token is hunter2-very-secret-value"),
        ("SYSTEM.md", "use recon"),
        (
            "skills/recon/SKILL.md",
            "---\nname: recon\ndescription: reconciling\n---\n",
        ),
    ]);
    let errors = validate(&mut tree, &rules).unwrap_err();
    assert!(
        has(&errors, "TOOL_GUIDANCE.md contains a secret"),
        "{errors:?}"
    );
    assert!(
        has(&errors, "skill 'recon' is already installed"),
        "{errors:?}"
    );
}

#[test]
fn keys_are_normalized_and_census_counts_components() {
    let mut tree = tree(&[
        ("./scripts/a.py", "x = 1\n"),
        (
            "agents/critic/AGENT.md",
            "---\nname: critic\ndescription: Reviews code\ntoolsets: [file]\n---\nBe strict.\n",
        ),
    ]);
    let validated = validate(&mut tree, &rules()).unwrap();
    assert!(tree.files.contains_key("scripts/a.py"));
    assert_eq!(validated.census.files, 6);
    assert_eq!(validated.census.agents, 1);
    assert_eq!(validated.census.scripts, 1);
}
