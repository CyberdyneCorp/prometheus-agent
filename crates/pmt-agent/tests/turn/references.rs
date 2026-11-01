//! `@` context references (spec: agent-turn-loop, Context references in
//! user input; Context reference safety and completion).

use std::path::{Path, PathBuf};

use pmt_agent::references::{ATTACHED, Expanded, Expander, Places, UrlSource};
use pmt_agent::turn::ReferenceSetup;
use pmt_agent::turn::model::BoxFuture;
use pmt_store::{Role, Source, Visibility};

use crate::support::{FakeTools, Profile, agent, mock_model};

struct Fixture {
    profile: Profile,
    work: PathBuf,
    places: Places,
}

fn fixture() -> Fixture {
    let profile = Profile::new("");
    let work = profile.path("work/app");
    let lines: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/main.rs"), lines.join("\n")).unwrap();
    let user_home = profile.path("user");
    std::fs::create_dir_all(user_home.join(".aws")).unwrap();
    std::fs::write(
        user_home.join(".aws/credentials"),
        "aws_secret_access_key = x",
    )
    .unwrap();
    let places = Places {
        cwd: work.clone(),
        workspace: work.clone(),
        user_home: Some(user_home),
        prometheus_home: Some(profile.home.root().to_path_buf()),
    };
    Fixture {
        profile,
        work,
        places,
    }
}

async fn expand(places: &Places, window: u64, text: &str) -> Expanded {
    Expander {
        places,
        context_window: window,
        urls: None,
    }
    .expand(text)
    .await
}

#[tokio::test]
async fn a_line_range_is_stored_with_the_user_message() {
    let f = fixture();
    let s = f.profile.session(Source::Cli);
    let model = mock_model(&f.profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let places = f.places.clone();
    let run = agent(&f.profile, s, model, FakeTools::new(), |setup| {
        setup.references = Some(ReferenceSetup { places, urls: None });
    });
    run.turn(&f.profile, "explain @file:src/main.rs:10-20")
        .await;
    let rows = f.profile.store.messages(s, Visibility::Active).unwrap();
    assert_eq!(rows[0].role, Role::User);
    let stored = rows[0].content.clone().unwrap();
    let (message, attached) = stored.split_once(ATTACHED).unwrap();
    assert_eq!(message, "explain @file:src/main.rs:10-20\n\n");
    let expected: Vec<String> = (10..=20).map(|n| format!("line {n}")).collect();
    assert!(
        attached.contains(&format!("```\n{}\n```", expected.join("\n"))),
        "{attached}"
    );
    assert!(!attached.contains("line 9\n") && !attached.contains("line 21"));
}

#[tokio::test]
async fn an_invalid_range_attaches_the_whole_file() {
    let f = fixture();
    let out = expand(&f.places, 128_000, "see @file:src/main.rs:20-10.").await;
    assert!(
        out.text.contains("line 1\n") && out.text.contains("line 100"),
        "{}",
        out.text
    );
}

#[tokio::test]
async fn credential_files_are_refused_inline() {
    let f = fixture();
    let out = expand(&f.places, 128_000, "check @file:~/.aws/credentials").await;
    assert!(!out.text.contains("aws_secret_access_key"));
    assert!(
        out.text
            .contains("[warning] @file:~/.aws/credentials: path is a sensitive credential file"),
        "{}",
        out.text
    );
    let secrets = f.profile.dir().join("secrets.toml");
    std::fs::write(&secrets, "X = \"y\"").unwrap();
    let out = expand(&f.places, 128_000, &format!("@file:{}", secrets.display())).await;
    assert_eq!(out.warnings.len(), 1);
    assert!(out.warnings[0].ends_with("path is a sensitive credential file"));
}

#[tokio::test]
async fn traversal_out_of_the_workspace_is_refused() {
    let f = fixture();
    let out = expand(&f.places, 128_000, "@file:../../etc/passwd").await;
    assert_eq!(
        out.warnings,
        ["@file:../../etc/passwd: path is outside the allowed workspace"]
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc", f.work.join("escape")).unwrap();
        let out = expand(&f.places, 128_000, "@file:escape/hosts").await;
        assert!(out.warnings[0].ends_with("path is outside the allowed workspace"));
    }
}

#[tokio::test]
async fn binary_files_are_refused() {
    let f = fixture();
    std::fs::write(f.work.join("blob"), b"ab\0cd").unwrap();
    std::fs::write(f.work.join("logo.png"), b"not really").unwrap();
    let out = expand(&f.places, 128_000, "@file:blob @file:logo.png").await;
    assert_eq!(
        out.warnings,
        [
            "@file:blob: file is binary",
            "@file:logo.png: file is binary"
        ]
    );
}

#[tokio::test]
async fn folders_list_entries_with_sizes_up_to_the_cap() {
    let f = fixture();
    let many = f.work.join("many");
    std::fs::create_dir_all(&many).unwrap();
    for i in 0..250 {
        std::fs::write(many.join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let out = expand(&f.places, 128_000, "@folder:src @folder:many").await;
    assert!(
        out.text.contains("### @folder:src\n- main.rs (791 B)"),
        "{}",
        out.text
    );
    let listing = out.text.split("### @folder:many\n").nth(1).unwrap();
    assert_eq!(listing.lines().count(), 201, "200 entries then the marker");
    assert!(listing.ends_with("- ..."));
}

#[tokio::test]
async fn above_half_the_window_nothing_is_attached() {
    let f = fixture();
    // ~1,500 tokens of listing: over half of 2,000, over a quarter of 4,000.
    let big = f.work.join("big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..150 {
        std::fs::write(big.join(format!("a-rather-long-file-name-{i:04}.txt")), "x").unwrap();
    }
    let out = expand(&f.places, 2_000, "summarise @folder:big").await;
    assert_eq!(out.text, "summarise @folder:big");
    assert_eq!(out.warnings.len(), 1);
    assert!(
        out.warnings[0].contains("exceed 50% of the context window"),
        "{:?}",
        out.warnings
    );

    let out = expand(&f.places, 4_000, "summarise @folder:big").await;
    assert!(out.text.contains(ATTACHED));
    assert!(out.warnings[0].contains("more than 25% of the context window"));
    assert!(
        out.text
            .contains("[warning] attached context uses more than 25%")
    );
}

struct FakeWeb;

impl UrlSource for FakeWeb {
    fn extract<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { Ok(format!("page text of {url}")) })
    }
}

#[tokio::test]
async fn urls_go_through_the_web_extraction_path() {
    let f = fixture();
    let out = Expander {
        places: &f.places,
        context_window: 128_000,
        urls: Some(&FakeWeb),
    }
    .expand("read @url:https://example.com/a.")
    .await;
    assert!(
        out.text
            .contains("### @url:https://example.com/a\npage text of https://example.com/a")
    );
    let out = expand(&f.places, 128_000, "@url:https://example.com").await;
    assert!(out.warnings[0].contains("web extraction is not available"));
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn diff_staged_and_git_history_attach_git_output() {
    let f = fixture();
    if !git(&f.work, &["init", "-q"]) {
        eprintln!("git unavailable; skipped");
        return;
    }
    assert!(git(&f.work, &["add", "."]));
    assert!(git(&f.work, &["commit", "-q", "-m", "first commit"]));
    std::fs::write(f.work.join("src/main.rs"), "changed").unwrap();
    std::fs::write(f.work.join("new.txt"), "staged text").unwrap();
    assert!(git(&f.work, &["add", "new.txt"]));
    let out = expand(&f.places, 128_000, "@diff @staged @git:50").await;
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let diff = out.text.split("### @staged").next().unwrap();
    assert!(diff.contains("+changed"), "{diff}");
    assert!(out.text.contains("+staged text"));
    assert!(out.text.contains("### @git:10\n"), "clamped to 10");
    assert!(out.text.contains("first commit"));
}
