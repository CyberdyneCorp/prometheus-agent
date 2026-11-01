//! `patch`: replace mode (unique match, fuzzy strategies, ambiguity) and
//! V4A (atomic multi-file patches naming the failing file).

use pmt_tools::files::patch::PatchArgs;
use pmt_tools::result::Status;
use serde_json::{Value, json};

use crate::golden;
use crate::support::{Fixture, body};

fn replace(path: &str, old: &str, new: &str) -> PatchArgs {
    PatchArgs::Replace {
        path: path.into(),
        old_string: old.into(),
        new_string: new.into(),
        replace_all: false,
    }
}

#[test]
fn a_unique_match_is_replaced_with_a_diff_and_lint() {
    let fx = Fixture::new();
    fx.write(
        "src/lib.rs",
        "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    let result = fx
        .session()
        .patch(&replace("src/lib.rs", "    a + b", "    a.wrapping_add(b)"));
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(
        fx.read("src/lib.rs"),
        "fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n"
    );
    golden::check("patch-replace.json", &pretty(&body(&result)));
}

#[test]
fn an_ambiguous_match_names_the_locations_and_changes_nothing() {
    let fx = Fixture::new();
    let content = "fn a() {\n    let x = 1;\n}\nfn b() {\n    let x = 1;\n}\n";
    fx.write("a.rs", content);
    let result = fx
        .session()
        .patch(&replace("a.rs", "let x = 1;", "let x = 2;"));
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "old_string matches 2 locations; add context or set replace_all=true"
    );
    assert_eq!(fx.read("a.rs"), content);
    let all = PatchArgs::Replace {
        path: "a.rs".into(),
        old_string: "let x = 1;".into(),
        new_string: "let x = 2;".into(),
        replace_all: true,
    };
    let reply = body(&fx.session().patch(&all));
    assert_eq!(reply["replacements"], 2);
    assert_eq!(fx.read("a.rs").matches("let x = 2;").count(), 2);
}

#[test]
fn fuzzy_matches_report_their_strategy_and_misses_show_the_closest_region() {
    let fx = Fixture::new();
    fx.write("q.py", "def greet():\n    print(\u{201C}hello\u{201D})\n");
    let reply = body(
        &fx.session()
            .patch(&replace("q.py", "print(\"hello\")", "print(\"hi\")")),
    );
    assert_eq!(reply["match_strategy"], "unicode_normalized");
    assert_eq!(fx.read("q.py"), "def greet():\n    print(\"hi\")\n");

    let miss = fx
        .session()
        .patch(&replace("q.py", "def farewell(name, times):", "def x():"));
    assert_eq!(miss.status, Status::Error);
    golden::check("patch-miss.txt", &miss.text_content());
}

const V4A_OK: &str = "*** Begin Patch
*** Update File: a.rs
@@
 fn a() {
-    1
+    2
 }
*** Add File: c.rs
+fn c() {}
*** Delete File: old.rs
*** End Patch";

#[test]
fn a_v4a_patch_changes_every_file() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n    1\n}\n");
    fx.write("old.rs", "fn old() {}\n");
    let result = fx.session().patch(&PatchArgs::V4a {
        patch: V4A_OK.into(),
    });
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(fx.read("a.rs"), "fn a() {\n    2\n}\n");
    assert_eq!(fx.read("c.rs"), "fn c() {}\n");
    assert!(!fx.path("old.rs").exists());
    golden::check("patch-v4a.json", &pretty(&body(&result)));
}

#[test]
fn a_v4a_patch_with_one_failing_hunk_changes_no_file_and_names_it() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n    1\n}\n");
    fx.write("b.rs", "fn b() {\n    10\n}\n");
    let patch = "*** Begin Patch
*** Update File: a.rs
@@
-    1
+    2
*** Update File: b.rs
@@
-    99
+    100
*** End Patch";
    let result = fx.session().patch(&PatchArgs::V4a {
        patch: patch.into(),
    });
    assert_eq!(result.status, Status::Error);
    let text = result.text_content();
    assert!(
        text.starts_with("patch not applied (no file was changed): b.rs: hunk 1 does not apply"),
        "{text}"
    );
    assert_eq!(fx.read("a.rs"), "fn a() {\n    1\n}\n");
    assert_eq!(fx.read("b.rs"), "fn b() {\n    10\n}\n");
}

#[test]
fn both_argument_shapes_are_accepted_whatever_was_advertised() {
    let shapes = [
        (
            json!({"path": "a", "old_string": "x", "new_string": "y"}),
            "replace",
        ),
        (
            json!({"mode": "replace", "path": "a", "old_string": "x", "new_string": "y"}),
            "replace",
        ),
        (json!({"patch": "*** Begin Patch"}), "v4a"),
        (json!({"mode": "v4a", "patch": "p"}), "v4a"),
    ];
    for (args, mode) in shapes {
        let parsed = PatchArgs::from_value(&args).unwrap();
        assert_eq!(
            matches!(parsed, PatchArgs::V4a { .. }),
            mode == "v4a",
            "{args}"
        );
    }
    assert!(
        PatchArgs::from_value(&json!({"mode": "v4a"}))
            .unwrap_err()
            .contains("'patch'")
    );
}

#[test]
fn patch_obeys_the_write_guards() {
    let fx = Fixture::new();
    let grants = fx.prometheus.join("profiles/default/grants.toml");
    let result = fx
        .session()
        .patch(&replace(grants.to_str().unwrap(), "# control", "x"));
    // A guard refusal is `denied`, as for write_file.
    assert_eq!(result.status, Status::Denied);
    assert_eq!(
        result.text_content(),
        "refusing to write Prometheus control file: grants.toml"
    );
}

const SECRET: &str = "OPENAI_API_KEY=sk-live-secret\nDEBUG=1\n";
const SECRET_REFUSAL: &str = "Refusing to read '.env.local': it typically holds secrets; ask the user to share the specific value you need";

/// The call is refused as a read, nothing of the file reaches the model
/// and the file is untouched.
fn assert_secret_refused(fx: &Fixture, result: &pmt_tools::result::ToolResult) {
    assert_eq!(result.status, Status::Denied);
    let text = result.text_content();
    assert_eq!(text, SECRET_REFUSAL);
    assert!(!text.contains("sk-live-secret") && !text.contains("DEBUG"));
    assert_eq!(fx.read(".env.local"), SECRET);
}

#[test]
fn patch_never_reveals_a_protected_secret_file() {
    let fx = Fixture::new();
    fx.write(".env.local", SECRET);
    let session = fx.session();
    // A hit would return the old line in its diff, a miss the closest
    // region of the file.
    let hit = session.patch(&replace(".env.local", "DEBUG=1", "DEBUG=0"));
    assert_secret_refused(&fx, &hit);
    let miss = session.patch(&replace(".env.local", "OPENAI_API_KEX=", "X="));
    assert_secret_refused(&fx, &miss);
    for patch in [
        "*** Begin Patch\n*** Update File: .env.local\n@@\n-DEBUG=1\n+DEBUG=0\n*** End Patch",
        "*** Begin Patch\n*** Delete File: .env.local\n*** End Patch",
    ] {
        let result = session.patch(&PatchArgs::V4a {
            patch: patch.into(),
        });
        assert_secret_refused(&fx, &result);
    }
}

#[cfg(unix)]
#[test]
fn a_link_to_a_secret_file_is_refused_too() {
    let fx = Fixture::new();
    fx.write(".env.local", SECRET);
    std::os::unix::fs::symlink(fx.path(".env.local"), fx.path("notes.txt")).unwrap();
    let result = fx
        .session()
        .patch(&replace("notes.txt", "DEBUG=1", "DEBUG=0"));
    assert_eq!(result.status, Status::Denied);
    assert!(!result.text_content().contains("sk-live-secret"));
    assert_eq!(fx.read(".env.local"), SECRET);
}

#[test]
fn secret_files_can_still_be_created_and_templates_patched() {
    let fx = Fixture::new();
    fx.write(".env.example", "OPENAI_API_KEY=\n");
    let session = fx.session();
    let template = session.patch(&replace(
        ".env.example",
        "OPENAI_API_KEY=",
        "OPENAI_API_KEY=changeme",
    ));
    assert_eq!(template.status, Status::Ok, "{}", template.text_content());
    assert_eq!(fx.read(".env.example"), "OPENAI_API_KEY=changeme\n");
    let add = session.patch(&PatchArgs::V4a {
        patch: "*** Begin Patch\n*** Add File: .env\n+TOKEN=abc\n*** End Patch".into(),
    });
    assert_eq!(add.status, Status::Ok, "{}", add.text_content());
    assert_eq!(fx.read(".env"), "TOKEN=abc\n");
}

fn pretty(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}
