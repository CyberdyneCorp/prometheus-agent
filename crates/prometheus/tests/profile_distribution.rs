//! Profile descriptions, distributions and identity moves (spec:
//! configuration-and-profiles, Profile descriptions, Profile
//! distributions, Identity migration and purge) against the built binary.

mod common;

use std::path::Path;
use std::process::Command;

use common::{append_config, mock_home, ok, run, run_with_stdin, stderr, stdout};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A distribution repository: manifest, identity, one skill, an MCP
/// server, a cron job, an initial config and a stray `secrets.toml`.
fn distribution_repo(root: &Path) {
    std::fs::create_dir_all(root.join("skills/web-research")).unwrap();
    std::fs::write(
        root.join("distribution.toml"),
        "name = \"research-bot\"\nversion = \"1.0.0\"\ndescription = \"Finds sources.\"\n\
         prometheus_requires = \">=0.1.0\"\nauthor = \"t\"\nlicense = \"MIT\"\n\n\
         [[secrets_required]]\nname = \"EXA_API_KEY\"\ndescription = \"search\"\nrequired = true\n",
    )
    .unwrap();
    std::fs::write(root.join("SOUL.md"), "You are research-bot v1.\n").unwrap();
    std::fs::write(
        root.join("skills/web-research/SKILL.md"),
        "---\nname: web-research\ndescription: Research the web.\n---\nSearch carefully.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("mcp.toml"),
        "[servers.docs]\ncommand = \"docs-server\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("cron.toml"),
        "[[jobs]]\nid = \"digest\"\nname = \"digest\"\nschedule = \"every 1d\"\nprompt = \"Summarize the news.\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("config.toml"),
        "schema_version = 1\n[model]\ndefault = \"dist-model\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("secrets.toml"),
        "LEAKED_TOKEN = \"sk-leaked-0123456789\"\n",
    )
    .unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "v1"]);
}

fn head(root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn install_previews_never_copies_credentials_and_update_keeps_user_config() {
    let home = mock_home();
    let repo = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    distribution_repo(repo.path());
    let url = format!("file://{}", repo.path().display());

    // Without -y and without a terminal nothing is installed (exit 4).
    let refused = run(home.path(), &["profile", "install", &url]);
    assert_eq!(refused.status.code(), Some(4), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("never installed: secrets.toml"));

    let out = run_with_stdin(
        home.path(),
        &["profile", "install", &url, "-y"],
        "exa-secret\n",
    );
    ok(&out);
    let profile = home.path().join("profiles/research-bot");
    assert_eq!(
        std::fs::read_to_string(profile.join("SOUL.md")).unwrap(),
        "You are research-bot v1.\n"
    );
    assert!(profile.join("skills/web-research/SKILL.md").is_file());
    // Credentials never installed: the stray file's contents are nowhere.
    let secrets = std::fs::read_to_string(profile.join("secrets.toml")).unwrap_or_default();
    assert!(!secrets.contains("sk-leaked"), "{secrets}");
    assert!(secrets.contains("EXA_API_KEY"));
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(
        config.contains("dist-model") && config.contains("docs-server"),
        "{config}"
    );
    let jobs = ok(&run(
        home.path(),
        &["-p", "research-bot", "--json", "cron", "list", "--all"],
    ));
    assert!(jobs.contains("digest") && jobs.contains("paused"), "{jobs}");

    // The user changes model.default; the distribution ships v2.
    ok(&run(
        home.path(),
        &[
            "-p",
            "research-bot",
            "config",
            "set",
            "model.default",
            "users-model",
        ],
    ));
    std::fs::write(repo.path().join("SOUL.md"), "You are research-bot v2.\n").unwrap();
    std::fs::write(
        repo.path().join("distribution.toml"),
        std::fs::read_to_string(repo.path().join("distribution.toml"))
            .unwrap()
            .replace("1.0.0", "2.0.0"),
    )
    .unwrap();
    git(repo.path(), &["commit", "-qam", "v2"]);
    ok(&run(
        home.path(),
        &["profile", "update", "research-bot", "-y"],
    ));
    let got = ok(&run(
        home.path(),
        &["-p", "research-bot", "config", "get", "model.default"],
    ));
    assert_eq!(got.trim(), "users-model");
    assert_eq!(
        std::fs::read_to_string(profile.join("SOUL.md")).unwrap(),
        "You are research-bot v2.\n"
    );
    let info = ok(&run(home.path(), &["profile", "info", "research-bot"]));
    assert!(info.contains(&head(repo.path())), "{info}");
    assert!(info.contains("2.0.0") && info.contains(&url), "{info}");
    // The paused job stays paused and is not duplicated.
    let jobs = ok(&run(
        home.path(),
        &["-p", "research-bot", "--json", "cron", "list", "--all"],
    ));
    assert_eq!(jobs.matches("\"digest\"").count(), 1, "{jobs}");
}

#[test]
fn an_incompatible_distribution_is_refused_with_78() {
    let home = mock_home();
    let repo = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(
        repo.path().join("distribution.toml"),
        "name = \"future\"\nversion = \"1.0.0\"\nprometheus_requires = \">=99.0.0\"\n",
    )
    .unwrap();
    let out = run(
        home.path(),
        &["profile", "install", repo.path().to_str().unwrap(), "-y"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(!home.path().join("profiles/future").exists());
}

#[test]
fn auto_description_does_not_overwrite() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["profile", "create", "research", "--no-alias"],
    ));
    ok(&run(
        home.path(),
        &["profile", "describe", "research", "--text", "My own words."],
    ));
    let out = run(home.path(), &["profile", "describe", "research", "--auto"]);
    ok(&out);
    assert!(stdout(&out).contains("description exists; use --overwrite"));
    let shown = ok(&run(home.path(), &["profile", "show", "research"]));
    assert!(shown.contains("My own words."), "{shown}");

    // --auto with --overwrite asks the profile route of the mock model.
    let profile = home.path().join("profiles/research");
    std::fs::write(
        profile.join("mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"A research assistant.\"\n",
    )
    .unwrap();
    std::fs::write(
        profile.join("config.toml"),
        "schema_version = 1\n[model]\nprovider = \"mock\"\n[providers.mock]\nscript = \"mock.toml\"\n\
         [auxiliary.profile]\nprovider = \"mock\"\nmodel = \"describer\"\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &["profile", "describe", "research", "--auto", "--overwrite"],
    ));
    let listed = ok(&run(home.path(), &["profile", "list"]));
    assert!(listed.contains("A research assistant."), "{listed}");
    let assignees = ok(&run(home.path(), &["kanban", "assignees"]));
    assert!(assignees.contains("A research assistant."), "{assignees}");
}

#[test]
fn purge_is_snapshotted() {
    let home = mock_home();
    append_config(home.path(), "");
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    let soul = home.path().join("profiles/work/SOUL.md");
    std::fs::write(&soul, "You are work.\n").unwrap();
    ok(&run(
        home.path(),
        &[
            "-p",
            "work",
            "memory",
            "add",
            "--target",
            "user",
            "The user is Ana.",
        ],
    ));
    ok(&run(
        home.path(),
        &["profile", "identity", "purge", "work", "--yes"],
    ));
    assert!(!soul.exists());
    let snapshots = ok(&run(
        home.path(),
        &["-p", "work", "--json", "snapshots", "list"],
    ));
    assert!(
        snapshots.contains("pre-identity-purge") && snapshots.contains("manual"),
        "{snapshots}"
    );
    let memory = ok(&run(
        home.path(),
        &["-p", "work", "--json", "memory", "list", "--all"],
    ));
    assert!(!memory.contains("The user is Ana."), "{memory}");
}

#[test]
fn identity_moves_to_another_profile() {
    let home = mock_home();
    for name in ["old", "new"] {
        ok(&run(
            home.path(),
            &["profile", "create", name, "--no-alias"],
        ));
    }
    std::fs::write(home.path().join("profiles/old/SOUL.md"), "You are old.\n").unwrap();
    ok(&run(
        home.path(),
        &[
            "-p",
            "old",
            "memory",
            "add",
            "--target",
            "user",
            "The user likes tea.",
        ],
    ));
    ok(&run(
        home.path(),
        &["profile", "identity", "move", "old", "new", "--yes"],
    ));
    assert_eq!(
        std::fs::read_to_string(home.path().join("profiles/new/SOUL.md")).unwrap(),
        "You are old.\n"
    );
    let memory = ok(&run(
        home.path(),
        &["-p", "new", "--json", "memory", "list", "--all"],
    ));
    assert!(memory.contains("The user likes tea."), "{memory}");
}
