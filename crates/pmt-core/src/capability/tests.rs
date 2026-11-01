use std::path::PathBuf;

use proptest::prelude::*;

use super::{Access, Capability, CapabilitySet, Places};

fn cap(text: &str) -> Capability {
    Capability::parse(text).unwrap()
}

fn set(texts: &[&str]) -> CapabilitySet {
    CapabilitySet::parse(texts).unwrap()
}

#[test]
fn unknown_capability_is_a_load_error_naming_the_string() {
    let err = Capability::parse("filesystem.write:/tmp").unwrap_err();
    assert_eq!(err.message(), "unknown capability 'filesystem.write:/tmp'");
    assert_eq!(err.code().code, "capability.unknown");
    assert_eq!(err.exit_code(), 78);
    for bad in [
        "fs.read:",
        "fs.read:relative/**",
        "fs.write:<workspace>/../etc/**",
        "fs.read:<toolchain>/bin",
        "net.listen:anywhere",
        "net.connect:host:port",
        "net.connect:a*b.com",
        "secrets.read:lower",
        "tools.call:a b",
        "process.spawn:g*t",
        "ui.",
        "llm",
    ] {
        assert!(Capability::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn every_vocabulary_form_round_trips() {
    for text in [
        "fs.read:<workspace>/**",
        "fs.write:~/notes/*.md",
        "fs.read:<toolchain>",
        "fs.read:C:/Users/me/**",
        "net.connect:*",
        "net.connect:*.example.com:443",
        "net.connect:[::1]:8080",
        "net.listen:loopback",
        "net.listen:loopback:9000",
        "process.spawn:*",
        "process.spawn:git",
        "secrets.read:GITHUB_TOKEN",
        "llm.call",
        "llm.call:main",
        "tools.call:terminal",
        "tools.override:read_file",
        "ui.tui",
        "browser.real_profile",
    ] {
        assert_eq!(cap(text).to_string(), text);
    }
    assert_eq!(cap("fs.read:/a/b/").to_string(), "fs.read:/a/b");
    assert_eq!(
        cap("net.connect:API.Example.com").to_string(),
        "net.connect:api.example.com"
    );
}

#[test]
fn covers_follows_the_vocabulary() {
    let yes = [
        ("fs.write:<workspace>/**", "fs.read:<workspace>/src/a.rs"),
        ("fs.read:/etc", "fs.read:/etc/hosts"),
        ("fs.read:/**", "fs.read:/etc/*.conf"),
        ("net.connect:*", "net.connect:api.example.com:443"),
        (
            "net.connect:*.example.com",
            "net.connect:*.a.example.com:80",
        ),
        ("net.listen:loopback", "net.listen:loopback:9000"),
        ("process.spawn:*", "process.spawn:git"),
        ("llm.call", "llm.call:main"),
        ("tools.call:*", "tools.call:terminal"),
    ];
    for (a, b) in yes {
        assert!(cap(a).covers(&cap(b)), "{a} covers {b}");
    }
    let no = [
        ("fs.read:<workspace>/**", "fs.write:<workspace>/a"),
        ("fs.read:/etc", "fs.read:/etcetera"),
        ("fs.read:/etc/*.conf", "fs.read:/etc/a.conf"),
        ("fs.read:/**", "fs.read:<workspace>/**"),
        ("net.connect:*.example.com", "net.connect:example.com"),
        (
            "net.connect:api.example.com:443",
            "net.connect:api.example.com",
        ),
        ("llm.call:main", "llm.call"),
        ("secrets.read:A_KEY", "secrets.read:B_KEY"),
    ];
    for (a, b) in no {
        assert!(!cap(a).covers(&cap(b)), "{a} must not cover {b}");
    }
}

/// Spec scenario "Subagent asks for more".
#[test]
fn a_child_cannot_ask_for_more_than_its_parent() {
    let parent = set(&["fs.read:<workspace>/**", "net.connect:api.example.com:443"]);
    let err = parent.narrow(&set(&["net.connect:*"])).unwrap_err();
    assert_eq!(err.message(), "capability not granted: net.connect:*");
    assert_eq!(err.exit_code(), 77);
    let child = parent
        .narrow(&set(&["fs.read:<workspace>/src/**"]))
        .unwrap();
    assert_eq!(child.to_strings(), ["fs.read:<workspace>/src/**"]);
}

#[test]
fn intersection_keeps_only_what_both_allow() {
    let manifest = set(&[
        "net.connect:*.tracker.example",
        "fs.write:<workspace>/**",
        "llm.call",
    ]);
    let approved = set(&[
        "net.connect:api.tracker.example:443",
        "fs.read:<workspace>/**",
    ]);
    assert_eq!(
        manifest.intersect(&approved).to_strings(),
        [
            "fs.read:<workspace>/**",
            "net.connect:api.tracker.example:443"
        ]
    );
}

fn places(root: &std::path::Path) -> Places {
    Places {
        user_home: root.join("home"),
        workspace: root.join("work"),
        scratch: vec![root.join("scratch")],
        toolchain: vec![root.join("usr")],
    }
}

/// Spec scenario "Symlink escape".
#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_workspace_is_checked_at_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let places = places(temp.path());
    let outside = temp.path().join("etc");
    std::fs::create_dir_all(&places.workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, places.workspace.join("link")).unwrap();
    let grants = set(&["fs.write:<workspace>/**"]);

    let escaped = places.workspace.join("link/passwd");
    assert!(!grants.allows_path(Access::Write, &escaped, &places));
    let err = grants
        .require_path(Access::Write, &escaped, &places)
        .unwrap_err();
    let target = super::canonical(&outside).join("passwd");
    assert_eq!(
        err.message(),
        format!("capability not granted: fs.write:{}", target.display())
    );
    // Climbing back through `..` after a missing component still resolves the link.
    let sneaky = places.workspace.join("missing/../link/passwd");
    assert!(!grants.allows_path(Access::Write, &sneaky, &places));
    assert!(grants.allows_path(Access::Write, &places.workspace.join("new/file"), &places));
}

/// A grant made from a run-time directory reads exactly that subtree, even
/// when the directory name holds glob characters.
#[test]
fn read_under_grants_reading_one_subtree_only() {
    let dir = PathBuf::from("/nonexistent-p/cache/we[b]*");
    let grant = Capability::read_under(&dir);
    assert_eq!(Capability::parse(&grant.to_string()).unwrap(), grant);
    let grants: CapabilitySet = std::iter::once(grant).collect();
    let places = Places::default();
    assert!(grants.allows_path(Access::Read, &dir, &places));
    assert!(grants.allows_path(Access::Read, &dir.join("a/b.md"), &places));
    assert!(!grants.allows_path(Access::Write, &dir.join("b.md"), &places));
    let sibling = PathBuf::from("/nonexistent-p/cache/web-other/b.md");
    assert!(!grants.allows_path(Access::Read, &sibling, &places));
}

#[test]
fn path_checks_honour_access_placeholders_and_globs() {
    let temp = tempfile::tempdir().unwrap();
    let places = places(temp.path());
    for dir in [&places.workspace, &places.user_home, &places.toolchain[0]] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let grants = set(&[
        "fs.read:<toolchain>",
        "fs.write:<scratch>/**",
        "fs.read:~/notes/*.md",
        "fs.write:<workspace>/out",
    ]);
    let ok = |access, path: PathBuf| grants.allows_path(access, &path, &places);
    assert!(ok(Access::Read, places.toolchain[0].join("bin/cc")));
    assert!(!ok(Access::Write, places.toolchain[0].join("bin/cc")));
    assert!(ok(Access::Write, places.scratch[0].join("a/b")));
    assert!(ok(Access::Read, places.user_home.join("notes/a.md")));
    assert!(!ok(Access::Read, places.user_home.join("notes/sub/a.md")));
    assert!(ok(Access::Write, places.workspace.join("out/x.txt")));
    assert!(!ok(Access::Write, places.workspace.join("outside")));
}

fn any_capability() -> impl Strategy<Value = Capability> {
    let texts = [
        "fs.read:<workspace>/**",
        "fs.read:<workspace>/src/**",
        "fs.write:<workspace>/**",
        "fs.write:<workspace>/src/a.rs",
        "fs.read:<workspace>/src/*.rs",
        "fs.read:~/**",
        "fs.read:/etc/**",
        "fs.read:/etc/*.conf",
        "fs.read:/**",
        "fs.read:<toolchain>",
        "net.connect:*",
        "net.connect:*.example.com",
        "net.connect:*.b.example.com:443",
        "net.connect:a.b.example.com:443",
        "net.connect:api.example.com",
        "net.listen:loopback",
        "net.listen:loopback:9000",
        "process.spawn:*",
        "process.spawn:git",
        "secrets.read:A_KEY",
        "secrets.read:B_KEY",
        "llm.call",
        "llm.call:main",
        "tools.call:*",
        "tools.call:terminal",
        "tools.override:read_file",
        "ui.tui",
        "browser.real_profile",
    ];
    proptest::sample::select(texts.to_vec()).prop_map(cap)
}

fn any_set() -> impl Strategy<Value = CapabilitySet> {
    proptest::collection::vec(any_capability(), 0..8).prop_map(CapabilitySet::from_iter)
}

proptest! {
    /// Covering is transitive, so "allowed by a member" is a sound order
    /// for every resolution below.
    #[test]
    fn covers_is_transitive(a in any_capability(), b in any_capability(), c in any_capability()) {
        if a.covers(&b) && b.covers(&c) {
            prop_assert!(a.covers(&c), "{a} > {b} > {c}");
        }
    }

    /// Intersection never allows anything either side refuses.
    #[test]
    fn intersection_never_widens(a in any_set(), b in any_set(), probe in any_capability()) {
        let both = a.intersect(&b);
        if both.allows(&probe) {
            prop_assert!(a.allows(&probe) && b.allows(&probe), "{probe}");
        }
    }

    /// A narrowed set (a subagent's) allows nothing its parent refuses, at
    /// any delegation depth.
    #[test]
    fn narrowing_never_widens(
        root in any_set(),
        first in any_set(),
        second in any_set(),
        probe in any_capability(),
    ) {
        if let Ok(child) = root.narrow(&first) {
            prop_assert!(!child.allows(&probe) || root.allows(&probe));
            if let Ok(grandchild) = child.narrow(&second) {
                prop_assert!(!grandchild.allows(&probe) || root.allows(&probe));
            }
        } else {
            prop_assert!(!root.missing(&first).is_empty());
        }
    }

    /// Removing a capability (revocation) leaves nothing it covered.
    #[test]
    fn revocation_never_widens(a in any_set(), revoked in any_capability(), probe in any_capability()) {
        let left = a.without(&revoked);
        prop_assert!(!left.allows(&probe) || a.allows(&probe));
        prop_assert!(!left.iter().any(|held| revoked.covers(held)));
    }

    /// The tool a session may use is within the session and the tool's
    /// declaration, also for concrete paths.
    #[test]
    fn path_access_of_an_intersection_is_within_both(
        a in any_set(),
        b in any_set(),
        rel in proptest::sample::select(vec!["src/a.rs", "src/b.txt", "out/x", "a.rs"]),
        write in any::<bool>(),
    ) {
        let places = Places {
            user_home: PathBuf::from("/nonexistent-h"),
            workspace: PathBuf::from("/nonexistent-w"),
            scratch: vec![PathBuf::from("/nonexistent-s")],
            toolchain: vec![PathBuf::from("/nonexistent-t")],
        };
        let access = if write { Access::Write } else { Access::Read };
        let path = places.workspace.join(rel);
        if a.intersect(&b).allows_path(access, &path, &places) {
            prop_assert!(a.allows_path(access, &path, &places));
            prop_assert!(b.allows_path(access, &path, &places));
        }
    }
}
