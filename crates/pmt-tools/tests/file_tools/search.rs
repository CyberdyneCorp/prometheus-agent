//! `search_files`: content and file modes, count mode, limits, ignores.

use pmt_tools::files::search::{OutputMode, SearchArgs, Target};
use pmt_tools::result::Status;
use serde_json::json;

use crate::golden;
use crate::support::{Fixture, body};

fn project() -> Fixture {
    let fx = Fixture::new();
    fx.write(".gitignore", "target/\n*.log\n");
    fx.write(
        "src/main.rs",
        "fn main() {\n    // TODO: parse args\n    run();\n}\n",
    );
    fx.write(
        "src/lib.rs",
        "// TODO: docs\npub fn run() {}\n// TODO: tests\n",
    );
    fx.write("README.md", "# demo\nNo todo here.\n");
    fx.write("target/debug/out.rs", "// TODO: ignored build output\n");
    fx.write("app.log", "TODO in a log\n");
    fx.write(".git/HEAD", "TODO: ref\n");
    fx.write("blob.bin", b"TODO\0\x01\x02");
    fx
}

#[test]
fn count_mode_lists_per_file_counts_and_no_content() {
    let fx = project();
    let mut args = SearchArgs::new("TODO");
    args.output_mode = OutputMode::Count;
    let result = fx.session().search_files(&args);
    assert_eq!(result.status, Status::Ok);
    let reply = body(&result);
    assert_eq!(
        reply["counts"],
        json!([{"path": "src/lib.rs", "count": 2}, {"path": "src/main.rs", "count": 1}])
    );
    assert_eq!(reply["total_matches"], 3);
    assert!(!result.text_content().contains("parse args"));
    golden::check("search-count.json", &format!("{reply:#}\n"));
}

#[test]
fn content_mode_gives_path_line_text_with_context() {
    let fx = project();
    let mut args = SearchArgs::new("TODO");
    args.context = 1;
    let reply = body(&fx.session().search_files(&args));
    golden::check("search-content.json", &format!("{reply:#}\n"));
    let matches = reply["matches"].as_array().unwrap();
    assert!(matches.contains(&json!("src/main.rs:2:    // TODO: parse args")));
    assert!(matches.contains(&json!("src/main.rs-1-fn main() {")));
    // .gitignore, VCS directories and binary files are skipped.
    let text = reply.to_string();
    for skipped in ["target/", "app.log", ".git", "blob.bin"] {
        assert!(!text.contains(skipped), "{skipped} in {text}");
    }
}

#[test]
fn limit_and_offset_page_the_results() {
    let fx = project();
    let mut args = SearchArgs::new("TODO");
    args.limit = 2;
    let first = body(&fx.session().search_files(&args));
    assert_eq!(first["matches"].as_array().unwrap().len(), 2);
    assert_eq!(
        (first["truncated"].clone(), first["next_offset"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(first["total_matches"], 3);
    args.offset = 2;
    let last = body(&fx.session().search_files(&args));
    assert_eq!(last["matches"].as_array().unwrap().len(), 1);
    assert_eq!(last["truncated"], false);
}

#[test]
fn files_mode_matches_names_by_glob_and_file_glob_filters_content() {
    let fx = project();
    let mut args = SearchArgs::new("*.rs");
    args.target = Target::Files;
    let reply = body(&fx.session().search_files(&args));
    assert_eq!(reply["files"], json!(["src/lib.rs", "src/main.rs"]));

    let mut args = SearchArgs::new("todo");
    args.file_glob = Some("*.md".into());
    args.output_mode = OutputMode::FilesOnly;
    assert_eq!(
        body(&fx.session().search_files(&args))["files"],
        json!(["README.md"])
    );
}

#[test]
fn an_invalid_regex_is_rejected_before_searching() {
    let fx = project();
    let result = fx.session().search_files(&SearchArgs::new("fn ("));
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().starts_with("invalid pattern: "),
        "{}",
        result.text_content()
    );
}

#[test]
fn arguments_are_bounded() {
    let args = SearchArgs::from_value(&json!({"pattern": "x", "limit": 9999, "context": 50}));
    assert_eq!((args.limit, args.context), (500, 10));
    let args = SearchArgs::from_value(&json!({"pattern": "x"}));
    assert_eq!((args.limit, args.offset, args.context), (50, 0, 0));
}
