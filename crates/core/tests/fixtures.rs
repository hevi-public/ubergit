//! Discovery and summaries against real repos built by `scripts/make-fixtures.sh`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ubergit_core::discovery;
use ubergit_core::summary::summarize;
use ubergit_core::*;

fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-fixtures.sh");
        let status = std::process::Command::new("bash")
            .arg(script)
            .arg(&dir)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run make-fixtures.sh");
        assert!(status.success(), "make-fixtures.sh failed");
        dir.canonicalize().unwrap()
    })
}

fn git() -> Git {
    Git::new(Arc::new(())).with_env([("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")])
}

fn locations() -> &'static Vec<RepoLocation> {
    static REPOS: OnceLock<Vec<RepoLocation>> = OnceLock::new();
    REPOS.get_or_init(|| {
        async_io::block_on(discovery::discover(&git(), fixtures(), 3))
            .into_iter()
            .map(|(candidate, loc)| loc.unwrap_or_else(|e| panic!("{candidate:?}: {e}")))
            .collect()
    })
}

fn repo(name: &str) -> &'static RepoLocation {
    locations()
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("repo {name} not discovered"))
}

fn summary(name: &str) -> RepoSummary {
    async_io::block_on(summarize(&git(), repo(name))).unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

fn tracking(name: &str, ahead: u32, behind: u32) -> Upstream {
    Upstream::Tracking { name: name.into(), ahead, behind }
}

fn base(name: &str, ahead: u32, behind: u32) -> Option<BaseDivergence> {
    Some(BaseDivergence { base: name.into(), ahead, behind })
}

#[test]
fn discovers_every_repo_once() {
    let names: Vec<&str> = locations().iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "ahead", "bare-repo.git", "behind", "detached", "dirty", "diverged", "feature",
            "gone", "group/nested-svc", "legacy", "merging", "no-remote", "no-upstream",
            "rebasing", "shallow", "stashed", "synced", "unborn", "with-submodule", "wt-linked",
            "wt-main",
        ]
    );
    assert!(repo("bare-repo.git").bare);
    assert!(repo("wt-linked").is_linked_worktree());
    assert_eq!(repo("wt-linked").common_dir, repo("wt-main").git_dir);
    assert!(!repo("wt-main").is_linked_worktree());
}

#[test]
fn synced_ahead_behind_diverged() {
    let s = summary("synced");
    assert_eq!(s.head, Head::Branch("main".into()));
    assert_eq!(s.upstream, tracking("origin/main", 0, 0));
    assert_eq!(s.base, base("origin/main", 0, 0));
    assert!(s.is_settled());
    assert!(s.last_fetch.is_none(), "fresh clone has no FETCH_HEAD");

    assert_eq!(summary("ahead").upstream, tracking("origin/main", 1, 0));
    assert_eq!(summary("behind").upstream, tracking("origin/main", 0, 2));
    assert!(summary("behind").last_fetch.is_some());
    let diverged = summary("diverged");
    assert_eq!(diverged.upstream, tracking("origin/main", 1, 2));
    assert_eq!(diverged.base, base("origin/main", 1, 2));
    assert!(!diverged.is_settled());
}

#[test]
fn dirty_counts() {
    let s = summary("dirty");
    assert_eq!(s.changes, ChangeCounts { files: 3, staged: 1, unstaged: 1, untracked: 1, conflicted: 0 });
}

#[test]
fn feature_branch_measured_against_default_branch() {
    let s = summary("feature");
    assert_eq!(s.head, Head::Branch("feature".into()));
    assert_eq!(s.upstream, tracking("origin/feature", 0, 0));
    assert_eq!(s.base, base("origin/main", 2, 1));
}

#[test]
fn detached_and_unborn() {
    let s = summary("detached");
    assert!(matches!(s.head, Head::Detached(ref oid) if oid.len() == 40));
    assert_eq!(s.upstream, Upstream::None);
    assert_eq!(s.base, base("origin/main", 0, 1));

    let s = summary("unborn");
    assert_eq!(s.head, Head::Unborn("main".into()));
    assert_eq!(s.base, None);
    assert_eq!(s.changes.untracked, 1);
    assert!(!s.has_remote());
}

#[test]
fn no_upstream_gone_no_remote() {
    let s = summary("no-upstream");
    assert_eq!(s.upstream, Upstream::None);
    assert_eq!(s.base, base("origin/main", 1, 0));

    let s = summary("gone");
    assert_eq!(s.upstream, Upstream::Gone { name: "origin/old-feature".into() });
    assert_eq!(s.base, base("origin/main", 1, 0));

    let s = summary("no-remote");
    assert!(!s.has_remote());
    assert_eq!(s.upstream, Upstream::None);
    assert_eq!(s.base, base("main", 0, 0));
}

#[test]
fn in_progress_operations() {
    let s = summary("rebasing");
    assert!(
        matches!(s.op, Some(RepoOp::Rebasing { step: Some((1, 1)), branch: Some(ref b) }) if b == "main"),
        "{:?}",
        s.op
    );
    assert_eq!(s.changes.conflicted, 1);

    let s = summary("merging");
    assert_eq!(s.op, Some(RepoOp::Merging));
    assert_eq!(s.changes.conflicted, 1);
}

#[test]
fn legacy_master_without_origin_head() {
    let s = summary("legacy");
    assert_eq!(s.head, Head::Branch("master".into()));
    assert_eq!(s.base, base("origin/master", 0, 0));
}

#[test]
fn stash_shallow_bare_worktree_submodule_nested() {
    assert_eq!(summary("stashed").stash_count, 2);
    assert!(summary("shallow").shallow);
    assert!(!summary("synced").shallow);

    let s = summary("bare-repo.git");
    assert_eq!(s.head, Head::Unborn("main".into()));
    assert!(s.changes.is_clean());

    let s = summary("wt-linked");
    assert_eq!(s.head, Head::Branch("wt-branch".into()));
    assert_eq!(s.base, base("origin/main", 0, 0));

    let s = summary("with-submodule");
    assert_eq!(s.upstream, tracking("origin/main", 1, 0));
    assert!(s.changes.is_clean());

    assert_eq!(summary("group/nested-svc").upstream, tracking("origin/main", 0, 0));
}
