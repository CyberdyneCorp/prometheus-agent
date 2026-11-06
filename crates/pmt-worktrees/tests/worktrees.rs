//! Worktree creation, the sweep, archives and restore against real git
//! repositories (spec: git-worktrees).

mod common;

use std::path::Path;

use common::{Fixture, commit_all, git};
use pmt_worktrees::create::{self, Request};
use pmt_worktrees::sweep::{self, Action, Mode, Settings};
use pmt_worktrees::{Kind, Repo, archive, registry, tree};

const DAY: u64 = 86_400;

fn named(name: &str) -> Request<'_> {
    Request {
        name: Some(name),
        ..Request::default()
    }
}

/// Scenario: Repository with a post-checkout hook.
#[test]
fn a_post_checkout_hook_does_not_run_when_a_tree_is_created() {
    let fx = Fixture::new();
    let marker = fx.work.join("hook-ran");
    let hook = fx.work.join(".git/hooks/post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The hook works for ordinary git.
    git(&fx.work, &["checkout", "-q", "-b", "probe"]);
    assert!(marker.exists() || cfg!(windows));
    let _ = std::fs::remove_file(&marker);
    git(&fx.work, &["checkout", "-q", "main"]);
    let _ = std::fs::remove_file(&marker);

    let created = create::create(&fx.work, &named("hooked")).unwrap();
    assert!(created.tree.path.join("README.md").is_file());
    assert!(!marker.exists(), "the post-checkout hook ran");
    let exclude = std::fs::read_to_string(fx.work.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("/.worktrees/"), "{exclude}");
    assert_eq!(git(&fx.work, &["status", "--porcelain"]), "");
}

/// Scenario: Unnamed tree.
#[test]
fn an_unnamed_tree_gets_a_generated_name_and_branch() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &Request::default()).unwrap();
    let name = created.tree.name().to_owned();
    let hex = name.strip_prefix("prom-").unwrap();
    assert_eq!(hex.len(), 8);
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(created.tree.path, fx.work.join(".worktrees").join(&name));
    assert_eq!(
        created.tree.branch().as_deref(),
        Some(format!("prometheus/{name}").as_str())
    );
    assert_eq!(created.tree.meta.kind, Kind::Generated);
    assert_eq!(created.base, "origin/main");
}

#[test]
fn an_existing_name_is_refused() {
    let fx = Fixture::new();
    create::create(&fx.work, &named("exp")).unwrap();
    let err = create::create(&fx.work, &named("exp")).unwrap_err();
    assert_eq!(err.message(), "worktree 'exp' already exists");
    let bad = create::create(&fx.work, &named("a/b")).unwrap_err();
    assert_eq!(bad.code().code, "worktree.invalid_name");
}

#[test]
fn a_failed_fetch_falls_back_to_the_local_tip_with_a_notice() {
    let fx = Fixture::new();
    std::fs::write(fx.work.join("local.txt"), "x").unwrap();
    commit_all(&fx.work, "local only");
    git(
        &fx.work,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    let created = create::create(&fx.work, &named("offline")).unwrap();
    assert_eq!(created.base, "HEAD");
    assert!(
        created.notices[0].contains("could not fetch origin"),
        "{:?}",
        created.notices
    );
    assert!(created.tree.path.join("local.txt").is_file());
}

#[test]
fn a_bare_repository_has_no_checkout_to_branch_from() {
    // Any directory under the target is inside this project's own
    // repository, so "no repository" is modelled by a bare one (the CLI
    // test covers a directory outside every repository).
    let fx = Fixture::new();
    let err = create::create(&fx.origin, &Request::default()).unwrap_err();
    assert_eq!(err.code().code, "worktree.not_a_repository");
    assert!(tree::list(&Repo::discover(&fx.work).unwrap()).is_empty());
}

fn settings() -> Settings {
    Settings::default()
}

fn decision_of(decisions: &[sweep::Decision], name: &str) -> sweep::Decision {
    decisions
        .iter()
        .find(|d| d.tree.name() == name)
        .cloned()
        .unwrap()
}

/// Scenario: Scheduled sweep.
#[test]
fn the_scheduled_sweep_removes_an_idle_clean_merged_tree() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &Request::default()).unwrap();
    let profile = fx.profile();
    registry::record(&profile, &created.repo).unwrap();
    let name = created.tree.name().to_owned();

    // Within its tier nothing happens.
    let now = tree::now_secs();
    let swept = registry::sweep_all(&profile, &settings(), now, Mode::Apply);
    let kept = decision_of(swept[0].decisions.as_ref().unwrap(), &name);
    assert_eq!(kept.action, Action::Keep("recent-activity"));
    assert!(created.tree.path.exists());

    let swept = registry::sweep_all(&profile, &settings(), now + DAY + 3600, Mode::Apply);
    let removed = decision_of(swept[0].decisions.as_ref().unwrap(), &name);
    assert_eq!(removed.action, Action::Remove);
    assert!(!created.tree.path.exists());
    assert!(tree::list(&created.repo).is_empty());
}

#[test]
fn the_registry_drops_repositories_that_no_longer_exist() {
    let fx = Fixture::new();
    let other = Fixture::new();
    let profile = fx.profile();
    registry::record(&profile, &Repo::discover(&fx.work).unwrap()).unwrap();
    registry::record(&profile, &Repo::discover(&other.work).unwrap()).unwrap();
    std::fs::remove_dir_all(&other.work).unwrap();
    let repos = registry::repos(&profile);
    assert_eq!(repos.len(), 1);
    let text = std::fs::read_to_string(profile.join(registry::FILE)).unwrap();
    assert!(!text.contains(&*other.work.to_string_lossy()), "{text}");
}

/// Scenario: Live process inside.
#[test]
fn a_tree_a_process_works_in_is_never_touched() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &Request::default()).unwrap();
    std::fs::write(created.tree.path.join("wip.txt"), "x").unwrap();
    let mut server = sleeper(&created.tree.path);
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 30 * DAY,
        Mode::Apply,
    )
    .unwrap();
    let _ = server.kill();
    let _ = server.wait();
    assert_eq!(decisions[0].action, Action::Keep("live-process"));
    assert!(created.tree.path.join("wip.txt").exists());
}

#[cfg(unix)]
fn sleeper(dir: &Path) -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir)
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn sleeper(dir: &Path) -> std::process::Child {
    std::process::Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn a_tree_owned_by_a_live_session_is_never_touched() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &Request::default()).unwrap();
    let lock = tree::OwnerLock::take(&created.tree, Some("s1")).unwrap();
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 30 * DAY,
        Mode::DryRun,
    )
    .unwrap();
    assert_eq!(decisions[0].action, Action::Keep("live-owner"));
    drop(lock);
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 30 * DAY,
        Mode::DryRun,
    )
    .unwrap();
    assert_eq!(decisions[0].action, Action::Remove);
}

/// Scenario: Large untracked file.
#[test]
fn a_large_untracked_file_keeps_a_dirty_tree() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("big")).unwrap();
    // Sparse: 200 MB of length, almost no disk.
    let file = std::fs::File::create(created.tree.path.join("dump.bin")).unwrap();
    file.set_len(200 * 1024 * 1024).unwrap();
    drop(file);
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 4 * DAY,
        Mode::Apply,
    )
    .unwrap();
    assert_eq!(decisions[0].action, Action::Keep("keep-large-untracked"));
    assert!(created.tree.path.join("dump.bin").exists());
    let meta = tree::find(&created.repo, "big").unwrap().meta;
    assert_eq!(
        meta.last_decision.as_deref(),
        Some("kept: keep-large-untracked")
    );
}

#[test]
fn a_dirty_tree_past_its_tier_is_archived_then_removed_and_its_local_branch_deleted() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("lane-a")).unwrap();
    std::fs::write(created.tree.path.join("notes.txt"), "draft").unwrap();
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 4 * DAY,
        Mode::Apply,
    )
    .unwrap();
    assert_eq!(decisions[0].action, Action::ArchiveRemove);
    assert!(
        decisions[0]
            .outcome
            .starts_with("archived to refs/prometheus/archive/lane-a and removed"),
        "{}",
        decisions[0].outcome
    );
    assert!(!created.tree.path.exists());
    assert_eq!(
        git(&fx.work, &["branch", "--list", "prometheus/lane-a"]),
        ""
    );
    assert_eq!(archive::list(&created.repo).unwrap().len(), 1);
}

#[test]
fn archiving_off_keeps_dirty_trees() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("lane-k")).unwrap();
    std::fs::write(created.tree.path.join("notes.txt"), "draft").unwrap();
    let off = Settings {
        archive_dirty: false,
        ..settings()
    };
    let decisions =
        sweep::sweep(&created.repo, &off, tree::now_secs() + 4 * DAY, Mode::Apply).unwrap();
    assert_eq!(decisions[0].action, Action::Keep("keep-dirty"));
    assert!(created.tree.path.join("notes.txt").exists());
}

#[test]
fn a_dry_run_reports_every_decision_and_changes_nothing() {
    let fx = Fixture::new();
    let clean = create::create(&fx.work, &named("clean")).unwrap();
    let dirty = create::create(&fx.work, &named("dirty")).unwrap();
    std::fs::write(dirty.tree.path.join("x.txt"), "x").unwrap();
    let decisions = sweep::sweep(
        &clean.repo,
        &settings(),
        tree::now_secs() + 4 * DAY,
        Mode::DryRun,
    )
    .unwrap();
    assert_eq!(decision_of(&decisions, "clean").outcome, "remove");
    assert_eq!(decision_of(&decisions, "dirty").outcome, "archive");
    assert!(clean.tree.path.exists() && dirty.tree.path.exists());
    assert!(archive::list(&clean.repo).unwrap().is_empty());
}

/// Scenario: Index changes during archiving.
#[test]
fn an_index_change_during_archiving_saves_the_archive_and_keeps_the_tree() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("lane-x")).unwrap();
    let path = created.tree.path.clone();
    std::fs::write(path.join("a.txt"), "first").unwrap();
    std::fs::write(path.join("b.txt"), "second").unwrap();
    let outcome = sweep::archive_then_remove(
        &created.repo,
        &created.tree,
        "sweep",
        &settings(),
        false,
        &mut || {
            git(&path, &["add", "b.txt"]);
        },
    )
    .unwrap();
    assert!(
        outcome.ends_with("kept: changed during archiving"),
        "{outcome}"
    );
    assert!(path.join("a.txt").exists());
    let saved = archive::find(&created.repo, "lane-x").unwrap();
    assert_eq!(saved.reference, "refs/prometheus/archive/lane-x");
}

/// Scenario: Restore an archived lane.
#[test]
fn an_archived_lane_restores_with_its_staged_and_untracked_files() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("lane-x")).unwrap();
    let path = created.tree.path.clone();
    std::fs::write(path.join("committed.txt"), "c").unwrap();
    commit_all(&path, "work");
    std::fs::write(path.join("staged.txt"), "staged\r\nbytes\n").unwrap();
    git(&path, &["add", "staged.txt"]);
    std::fs::write(path.join("README.md"), "hello\nunstaged edit\n").unwrap();
    std::fs::create_dir_all(path.join("new/dir")).unwrap();
    std::fs::write(
        path.join("new/dir/untracked.bin"),
        [0u8, 159, 146, 150, 13, 10],
    )
    .unwrap();
    std::fs::write(path.join(".gitignore"), "ignored.log\n").unwrap();
    std::fs::write(path.join("ignored.log"), "noise").unwrap();
    let staged_before = git(&path, &["diff", "--cached", "--name-only"]);
    let status_before = git(&path, &["status", "--porcelain", "-uall"]);

    let outcome = sweep::archive_then_remove(
        &created.repo,
        &created.tree,
        "exit",
        &settings(),
        false,
        &mut || {},
    )
    .unwrap();
    assert!(outcome.ends_with("and removed"), "{outcome}");
    assert!(!path.exists());

    let restored = archive::restore(&created.repo, "lane-x", None).unwrap();
    assert_eq!(restored.path, path);
    assert_eq!(restored.branch.as_deref(), Some("prometheus/lane-x"));
    assert_eq!(
        git(&path, &["diff", "--cached", "--name-only"]),
        staged_before
    );
    assert_eq!(
        git(&path, &["status", "--porcelain", "-uall"]),
        status_before
    );
    assert_eq!(
        std::fs::read(path.join("staged.txt")).unwrap(),
        b"staged\r\nbytes\n"
    );
    assert_eq!(
        std::fs::read(path.join("new/dir/untracked.bin")).unwrap(),
        [0u8, 159, 146, 150, 13, 10]
    );
    assert!(!path.join("ignored.log").exists());
    assert!(tree::find(&created.repo, "lane-x").is_some());
}

#[test]
fn a_moved_branch_restores_detached_at_the_archived_head() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("lane-d")).unwrap();
    std::fs::write(created.tree.path.join("w.txt"), "w").unwrap();
    let head = git(&created.tree.path, &["rev-parse", "HEAD"]);
    let archived = archive::archive(&created.repo, &created.tree, "exit", u64::MAX).unwrap();
    assert!(!archived.changed);
    sweep::remove(&created.repo, &created.tree, true).unwrap();
    // The branch moves on after the archive.
    git(&fx.work, &["branch", "-f", "prometheus/lane-d", "main~0"]);
    std::fs::write(fx.work.join("later.txt"), "l").unwrap();
    commit_all(&fx.work, "later");
    git(&fx.work, &["branch", "-f", "prometheus/lane-d", "main"]);
    let restored = archive::restore(&created.repo, "lane-d", None).unwrap();
    assert_eq!(restored.branch, None);
    assert_eq!(git(&restored.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(restored.path.join("w.txt")).unwrap(),
        "w"
    );
}

#[test]
fn archives_store_raw_bytes_even_with_a_clean_filter() {
    let fx = Fixture::new();
    std::fs::write(fx.work.join(".gitattributes"), "*.txt filter=upper\n").unwrap();
    commit_all(&fx.work, "attributes");
    git(&fx.work, &["push", "-q", "origin", "main"]);
    git(&fx.work, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    let created = create::create(&fx.work, &named("raw")).unwrap();
    std::fs::write(created.tree.path.join("x.txt"), "lower case\n").unwrap();
    let archived = archive::archive(&created.repo, &created.tree, "exit", u64::MAX).unwrap();
    let blob = git(
        &fx.work,
        &["show", &format!("{}:x.txt", archived.archive.commit)],
    );
    assert_eq!(blob, "lower case");
}

#[test]
fn an_unknown_archive_is_not_found() {
    let fx = Fixture::new();
    let repo = Repo::discover(&fx.work).unwrap();
    let err = archive::restore(&repo, "nope", None).unwrap_err();
    assert_eq!(err.code().code, "worktree.archive_not_found");
    assert_eq!(err.exit_code(), 66);
}

#[test]
fn old_archives_expire_at_the_sweep() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("old")).unwrap();
    std::fs::write(created.tree.path.join("x"), "x").unwrap();
    archive::archive(&created.repo, &created.tree, "exit", u64::MAX).unwrap();
    sweep::remove(&created.repo, &created.tree, false).unwrap();
    let now = tree::now_secs();
    sweep::sweep(&created.repo, &settings(), now + DAY, Mode::Apply).unwrap();
    assert_eq!(archive::list(&created.repo).unwrap().len(), 1);
    sweep::sweep(&created.repo, &settings(), now + 31 * DAY, Mode::Apply).unwrap();
    assert!(archive::list(&created.repo).unwrap().is_empty());
}

#[test]
fn a_pushed_branch_survives_its_trees_removal() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("pushed")).unwrap();
    std::fs::write(created.tree.path.join("f.txt"), "f").unwrap();
    commit_all(&created.tree.path, "feature");
    git(
        &created.tree.path,
        &["push", "-q", "origin", "prometheus/pushed"],
    );
    let decisions = sweep::sweep(
        &created.repo,
        &settings(),
        tree::now_secs() + 4 * DAY,
        Mode::Apply,
    )
    .unwrap();
    assert_eq!(decisions[0].action, Action::Remove);
    assert_eq!(
        git(&fx.work, &["branch", "--list", "prometheus/pushed"]),
        "prometheus/pushed"
    );
}

#[test]
fn a_tree_whose_directory_is_gone_is_forgotten_with_its_branch_kept() {
    let fx = Fixture::new();
    let created = create::create(&fx.work, &named("gone")).unwrap();
    std::fs::remove_dir_all(&created.tree.path).unwrap();
    let decisions =
        sweep::sweep(&created.repo, &settings(), tree::now_secs(), Mode::Apply).unwrap();
    assert!(
        decisions[0].outcome.starts_with("forgotten"),
        "{}",
        decisions[0].outcome
    );
    assert!(tree::list(&created.repo).is_empty());
    git(
        &fx.work,
        &["rev-parse", "--verify", "refs/heads/prometheus/gone"],
    );
}
