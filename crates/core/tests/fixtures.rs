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

/// What git itself resolves `rev` to in a fixture repo, if anything.
fn rev_parse(name: &str, rev: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo(name).root)
        .args(["rev-parse", "-q", "--verify", rev])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn tracking(name: &str, ahead: u32, behind: u32) -> Upstream {
    Upstream::Tracking { name: name.into(), ahead, behind }
}

fn base(name: &str, ahead: u32, behind: u32) -> Option<BaseDivergence> {
    Some(BaseDivergence { base: name.into(), ahead, behind })
}

#[test]
fn discovers_every_repo_once() {
    let mut names: Vec<&str> = locations().iter().map(|r| r.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "ahead", "ambiguous-upstream", "bare-clone.git", "bare-repo.git", "behind",
            "detached", "dirty", "diverged", "feature", "gone", "group/nested-svc", "legacy",
            "local-upstream", "merging", "no-remote", "no-upstream", "pushed-no-upstream",
            "rebasing", "shallow", "stashed", "synced", "unborn", "with-submodule", "wt-away",
            "wt-linked", "wt-main", "wt-main/.claude/worktrees/wt-hidden",
        ]
    );
    assert!(repo("bare-repo.git").bare);
    assert!(repo("wt-linked").is_linked_worktree());
    assert_eq!(repo("wt-linked").common_dir, repo("wt-main").git_dir);
    assert!(!repo("wt-main").is_linked_worktree());
}

#[test]
fn finds_linked_worktrees_the_walk_misses() {
    let hidden = repo("wt-main/.claude/worktrees/wt-hidden");
    assert_eq!(hidden.root, fixtures().join("wt-main/.claude/worktrees/wt-hidden"));
    assert_eq!(hidden.git_dir, repo("wt-main").git_dir.join("worktrees/wt-hidden"));
    assert_eq!(hidden.common_dir, repo("wt-main").git_dir);
    assert_eq!(summary("wt-main/.claude/worktrees/wt-hidden").head, Head::Branch("wt-hidden".into()));

    // Outside the workdir: named by its directory.
    let away = repo("wt-away");
    assert!(!away.root.starts_with(fixtures()));
    assert!(away.is_linked_worktree());

    // The pruned worktree's directory is gone, so it isn't listed.
    assert!(locations().iter().all(|r| !r.name.contains("wt-pruned")));

    // Each linked worktree belongs to wt-main.
    let all: Vec<&RepoLocation> = locations().iter().collect();
    let parents = discovery::main_checkouts(&all);
    let main_ix = all.iter().position(|r| r.name == "wt-main").unwrap();
    for (repo, parent) in all.iter().zip(&parents) {
        let expected = repo.name.starts_with("wt-") && repo.name != "wt-main";
        assert_eq!(*parent == Some(main_ix), expected, "{}", repo.name);
        assert!(expected || parent.is_none(), "{}", repo.name);
    }
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
fn head_and_upstream_oids_match_git() {
    for repo in locations() {
        let s = summary(&repo.name);
        assert_eq!(s.head_oid, rev_parse(&repo.name, "HEAD"), "{}: HEAD", repo.name);
        assert_eq!(s.upstream_oid, rev_parse(&repo.name, "@{upstream}"), "{}: upstream", repo.name);
    }
    // Which ones resolve, so the comparisons above aren't all between Nones.
    let resolved = |name: &str| {
        let s = summary(name);
        (s.head_oid.is_some(), s.upstream_oid.is_some())
    };
    for name in ["synced", "ahead", "behind", "diverged", "feature", "local-upstream", "ambiguous-upstream"] {
        assert_eq!(resolved(name), (true, true), "{name}");
    }
    for name in ["no-upstream", "pushed-no-upstream", "gone", "detached", "bare-clone.git"] {
        assert_eq!(resolved(name), (true, false), "{name}");
    }
    for name in ["unborn", "bare-repo.git"] {
        assert_eq!(resolved(name), (false, false), "{name}");
    }
}

#[test]
fn local_and_ambiguous_upstreams() {
    let s = summary("local-upstream");
    assert_eq!(s.upstream, tracking("main", 1, 0));
    assert_eq!(s.upstream_oid, rev_parse("local-upstream", "refs/heads/main"));

    // The local branch `origin/main` sits one commit back, so picking it would show.
    let s = summary("ambiguous-upstream");
    assert_eq!(s.upstream, tracking("remotes/origin/main", 0, 0));
    assert_eq!(s.upstream_oid, rev_parse("ambiguous-upstream", "refs/remotes/origin/main"));
    assert_ne!(s.upstream_oid, rev_parse("ambiguous-upstream", "refs/heads/origin/main"));
}

#[test]
fn remote_branch_oid_is_the_tip_a_pr_would_come_from() {
    // With an upstream on a remote, that's the upstream.
    for name in ["synced", "ahead", "behind", "diverged", "feature", "with-submodule", "ambiguous-upstream"] {
        let s = summary(name);
        assert!(s.remote_branch_oid.is_some(), "{name}");
        assert_eq!(s.remote_branch_oid, s.upstream_oid, "{name}");
    }
    // Pushed without -u: no upstream, but the push still moved origin/wip.
    let s = summary("pushed-no-upstream");
    assert_eq!(s.upstream, Upstream::None);
    assert_eq!(s.remote_branch_oid, rev_parse("pushed-no-upstream", "refs/remotes/origin/wip"));
    assert!(s.remote_branch_oid.is_some());
    assert_ne!(s.remote_branch_oid, s.head_oid);
    // Never pushed, deleted on the remote, detached, no remote, or tracking a local branch.
    for name in ["no-upstream", "gone", "detached", "no-remote", "local-upstream", "unborn"] {
        assert_eq!(summary(name).remote_branch_oid, None, "{name}");
    }
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
