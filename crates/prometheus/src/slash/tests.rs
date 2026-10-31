//! Spec scenarios of the registry and its resolution (spec: cli).

use super::*;

fn registry() -> Registry {
    Registry::new().unwrap()
}

fn builtin_name(resolution: &Resolution) -> Option<(&'static str, &str)> {
    match resolution {
        Resolution::Builtin { command, args } => Some((command.name, args.as_str())),
        _ => None,
    }
}

/// Scenario: Unique prefix.
#[test]
fn a_unique_prefix_runs_the_command() {
    let r = registry();
    assert_eq!(
        builtin_name(&r.resolve("/comp", Surface::Cli)),
        Some(("compress", ""))
    );
    // Aliases and a missing slash resolve too.
    assert_eq!(
        builtin_name(&r.resolve("quit", Surface::Cli)),
        Some(("exit", ""))
    );
    assert_eq!(
        builtin_name(&r.resolve("/RESET", Surface::Cli)),
        Some(("new", ""))
    );
}

/// Scenario: Arguments keep their case.
#[test]
fn arguments_keep_their_case() {
    let r = registry();
    assert_eq!(
        builtin_name(&r.resolve("/TITLE My Project", Surface::Cli)),
        Some(("title", "My Project"))
    );
}

#[test]
fn ambiguous_and_unknown_tokens_are_explained() {
    let r = registry();
    let ambiguous = r.resolve("/re", Surface::Cli);
    let text = ambiguous.message().unwrap();
    assert!(text.starts_with("/re is ambiguous: "), "{text}");
    assert!(
        text.contains("/resume") && text.contains("/retry"),
        "{text}"
    );
    assert_eq!(
        r.resolve("/compresss", Surface::Cli).message().unwrap(),
        "Unknown command: /compresss (did you mean /compress?)"
    );
    assert_eq!(
        r.resolve("/zzzzzz", Surface::Cli).message().unwrap(),
        "Unknown command: /zzzzzz"
    );
}

/// Scenario: Gateway-unavailable command hidden from help.
#[test]
fn a_command_of_other_surfaces_is_hidden_and_refused() {
    let r = registry();
    assert!(!r.help(Surface::Gateway).contains("/theme"));
    assert!(r.help(Surface::Cli).contains("/theme"));
    assert_eq!(
        r.resolve("/theme", Surface::Gateway).message().unwrap(),
        "/theme is not available here"
    );
}

#[test]
fn help_is_generated_from_the_registry_grouped_by_category() {
    let r = registry();
    let help = r.help(Surface::Cli);
    for command in builtins::BUILTINS.iter().filter(|c| c.on(Surface::Cli)) {
        assert!(
            help.contains(&command.usage()),
            "{} missing:\n{help}",
            command.name
        );
    }
    let session = help.find("Session:").unwrap();
    let exit = help.find("Exit:").unwrap();
    assert!(session < exit);
}

/// Scenario: Plugin command collides with built-in.
#[test]
fn a_plugin_redeclaring_a_built_in_fails_and_the_built_in_stays() {
    let mut r = registry();
    let declaration = Declaration {
        name: "model".into(),
        ..Declaration::default()
    };
    let err = r.register(&declaration, Source::Plugin).unwrap_err();
    assert_eq!(
        err.message(),
        "slash command '/model' conflicts with built-in"
    );
    assert_eq!(
        builtin_name(&r.resolve("/model", Surface::Cli)),
        Some(("model", ""))
    );
}

#[test]
fn unknown_enum_values_fail_registration() {
    let mut r = registry();
    let declaration = Declaration {
        name: "deploy".into(),
        busy_policy: Some("sometimes".into()),
        ..Declaration::default()
    };
    let err = r.register(&declaration, Source::Plugin).unwrap_err();
    assert!(err.message().contains("unknown busy_policy 'sometimes'"));
}

/// Scenario: Skill shadowed by quick command (the registry half: one
/// warning naming both, the quick command wins whatever the order).
#[test]
fn a_quick_command_shadows_a_skill_of_the_same_name() {
    for order in [
        [Source::Skill, Source::Quick],
        [Source::Quick, Source::Skill],
    ] {
        let mut r = registry();
        for source in order {
            let declaration = Declaration {
                name: "deploy".into(),
                description: format!("{source:?}"),
                ..Declaration::default()
            };
            r.register(&declaration, source).unwrap();
        }
        match r.resolve("/deploy", Surface::Cli) {
            Resolution::External { command, .. } => assert_eq!(command.source, Source::Quick),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("quick command") && r.warnings[0].contains("skill"));
    }
}
