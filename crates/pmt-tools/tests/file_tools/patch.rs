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
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "refusing to write Prometheus control file: grants.toml"
    );
}

fn pretty(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}
