//! The desktop's git review pane against real repositories (spec:
//! desktop-app, Git review pane).

mod common;

use common::{Fixture, git};
use pmt_worktrees::review::{Scope, Tree};

fn paths(review: &pmt_worktrees::review::Review) -> Vec<&str> {
    review.files.iter().map(|f| f.path.as_str()).collect()
}

/// Scenario: Last-turn scope.
#[test]
fn last_turn_shows_edits_and_commits_made_during_the_turn_read_only() {
    let fx = Fixture::new();
    std::fs::write(fx.work.join("before.txt"), "edited before the turn\n").unwrap();
    let tree = Tree::at(&fx.work).unwrap();
    let start = tree.snapshot().unwrap();
    // The turn edits two files and commits one of them.
    std::fs::write(fx.work.join("README.md"), "hello\nworld\n").unwrap();
    std::fs::write(fx.work.join("new.txt"), "new\n").unwrap();
    git(&fx.work, &["add", "README.md"]);
    git(&fx.work, &["commit", "-q", "-m", "turn commit"]);
    let review = tree.review(Scope::LastTurn, Some(&start)).unwrap();
    assert_eq!(paths(&review), ["README.md", "new.txt"]);
    assert!(review.read_only);
    assert!(review.files[0].patch.contains("+world"));
    assert_eq!(review.files[1].status, "added");
    // The uncommitted scope is the one that may change things.
    let uncommitted = tree.review(Scope::Uncommitted, None).unwrap();
    assert!(!uncommitted.read_only);
    assert_eq!(paths(&uncommitted), ["before.txt", "new.txt"]);
    assert_eq!(review.branch.as_deref(), Some("main"));
    assert_eq!((review.ahead, review.behind), (1, 0));
    let branch = tree.review(Scope::Branch, None).unwrap();
    assert_eq!(paths(&branch), ["README.md"]);
    assert!(branch.read_only);
}

#[test]
fn stage_unstage_revert_and_commit_apply_to_uncommitted_changes() {
    let fx = Fixture::new();
    std::fs::write(fx.work.join("README.md"), "changed\n").unwrap();
    std::fs::write(fx.work.join("new.txt"), "new\n").unwrap();
    let tree = Tree::at(&fx.work).unwrap();
    tree.stage(&["new.txt".to_owned()]).unwrap();
    let review = tree.review(Scope::Uncommitted, None).unwrap();
    let staged: Vec<bool> = review.files.iter().map(|f| f.staged).collect();
    assert_eq!(staged, [false, true]);
    tree.unstage(&["new.txt".to_owned()]).unwrap();
    assert!(!tree.review(Scope::Uncommitted, None).unwrap().files[1].staged);
    tree.revert(&["README.md".to_owned(), "new.txt".to_owned()])
        .unwrap();
    assert!(
        tree.review(Scope::Uncommitted, None)
            .unwrap()
            .files
            .is_empty()
    );
    assert!(!fx.work.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fx.work.join("README.md")).unwrap(),
        "hello\n"
    );
    std::fs::write(fx.work.join("c.txt"), "c\n").unwrap();
    tree.stage(&["c.txt".to_owned()]).unwrap();
    let commit = tree.commit("add c").unwrap();
    assert_eq!(git(&fx.work, &["log", "-1", "--format=%s"]), "add c");
    assert_eq!(git(&fx.work, &["rev-parse", "HEAD"]), commit);
}

/// Snapshots taken at once (a turn starting while the pane reviews) each
/// see the whole tree.
#[test]
fn concurrent_snapshots_do_not_share_an_index() {
    let fx = Fixture::new();
    std::fs::write(fx.work.join("new.txt"), "new\n").unwrap();
    let tree = Tree::at(&fx.work).unwrap();
    let alone = tree.snapshot().unwrap();
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let tree = tree.clone();
            std::thread::spawn(move || tree.snapshot().unwrap())
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), alone);
    }
}
