//! Branch switching against real repos. These tests change the repos, so they get their
//! own copy of the fixtures, apart from the read-only tests in `fixtures.rs`. Each test
//! uses a different repo, so they can run in parallel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_io::block_on;
use ubergit_core::ops::{self, DefaultSwitch, Switched};
use ubergit_core::summary::summarize;
use ubergit_core::*;

fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures-ops");
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

const ISOLATED: [(&str, &str); 2] = [("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")];

fn git() -> Git {
    Git::new(Arc::new(())).with_env(ISOLATED)
}

fn repo(name: &str) -> &'static RepoLocation {
    static REPOS: OnceLock<Vec<RepoLocation>> = OnceLock::new();
    REPOS
        .get_or_init(|| {
            block_on(discovery::discover(&git(), fixtures(), 3))
                .into_iter()
                .map(|(candidate, loc)| loc.unwrap_or_else(|e| panic!("{candidate:?}: {e}")))
                .collect()
        })
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("repo {name} not discovered"))
}

fn summary(name: &str) -> RepoSummary {
    block_on(summarize(&git(), repo(name))).unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

/// Runs git directly in a fixture repo to set up a test.
fn setup(name: &str, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo(name).root)
        .args(args)
        .envs(ISOLATED)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} in {name}");
}

fn switch(name: &str, branch: &str) -> Switched {
    let s = summary(name);
    block_on(ops::switch_branch(&git(), repo(name), branch, &s.remotes)).unwrap()
}

fn to_default(name: &str) -> DefaultSwitch {
    let s = summary(name);
    let default = s.default_branch.as_ref().expect("default branch");
    block_on(ops::switch_to_default(&git(), repo(name), default, &s.remotes, s.head.branch_name())).unwrap()
}

#[test]
fn switches_to_an_existing_local_branch() {
    // `gone` is on old-feature; its local main is still there.
    assert_eq!(switch("gone", "main"), Switched::Local);
    assert_eq!(summary("gone").head, Head::Branch("main".into()));
}

#[test]
fn creates_a_tracking_branch_when_only_the_remote_has_it() {
    setup("no-upstream", &["branch", "-D", "main"]);
    assert_eq!(switch("no-upstream", "main"), Switched::Tracking("origin/main".into()));
    let after = summary("no-upstream");
    assert_eq!(after.head, Head::Branch("main".into()));
    assert_eq!(after.upstream, Upstream::Tracking { name: "origin/main".into(), ahead: 0, behind: 0 });
}

#[test]
fn a_missing_branch_changes_nothing() {
    // `team/x` exists, but a prefix of it is not a branch.
    setup("synced", &["branch", "team/x"]);
    assert_eq!(switch("synced", "nope"), Switched::Missing);
    assert_eq!(switch("synced", "team"), Switched::Missing);
    assert_eq!(summary("synced").head, Head::Branch("main".into()));
}

#[test]
fn fast_forwards_the_default_branch_when_already_on_it() {
    let result = to_default("behind");
    assert_eq!(
        result,
        DefaultSwitch {
            branch: "main".into(),
            switched: None,
            upstream: Some("origin/main".into()),
            ahead: 0,
            behind: 2,
            fast_forwarded: true,
        }
    );
    assert!(summary("behind").is_settled());
}

#[test]
fn switches_from_a_feature_branch_to_default_and_fast_forwards() {
    // `feature` is on feature; its local main is one commit behind origin/main.
    let result = to_default("feature");
    assert_eq!(result.switched, Some(Switched::Local));
    assert_eq!((result.ahead, result.behind, result.fast_forwarded), (0, 1, true));
    let after = summary("feature");
    assert_eq!(after.head, Head::Branch("main".into()));
    assert!(after.is_settled());
}

#[test]
fn leaves_a_diverged_default_branch_alone() {
    let before = summary("diverged");
    let result = to_default("diverged");
    assert_eq!(result.switched, None);
    assert_eq!((result.ahead, result.behind, result.fast_forwarded), (1, 2, false));
    assert_eq!(summary("diverged").upstream, before.upstream);
}

#[test]
fn default_branch_without_origin_head_or_remote() {
    let legacy = to_default("legacy");
    assert_eq!((legacy.branch.as_str(), legacy.switched), ("master", None));
    assert_eq!(legacy.upstream.as_deref(), Some("origin/master"));
    let local = to_default("no-remote");
    assert_eq!((local.branch.as_str(), local.upstream), ("main", None));
}

#[test]
fn default_branch_parts_handle_remotes_with_slashes() {
    let parts = |full_ref: &str, remotes: &[&str]| {
        let default = DefaultBranch { full_ref: full_ref.into(), short: summary::short_ref(full_ref) };
        let remotes: Vec<String> = remotes.iter().map(|r| r.to_string()).collect();
        ops::default_branch_parts(&default, &remotes)
    };
    assert_eq!(parts("refs/remotes/origin/main", &["origin"]), ("main".into(), Some("origin".into())));
    assert_eq!(parts("refs/remotes/team/up/develop", &["team", "team/up"]), ("develop".into(), Some("team/up".into())));
    assert_eq!(parts("refs/heads/master", &[]), ("master".into(), None));
}

#[test]
fn branch_name_validation() {
    for ok in ["main", "feat/x", "JIRA-123_fix", "v1.2"] {
        assert!(ops::is_valid_branch_name(ok), "{ok}");
    }
    for bad in ["", "-f", "a b", "a..b", "x.lock", "a~1", "feat/", "/x", "@{u}", "a:b"] {
        assert!(!ops::is_valid_branch_name(bad), "{bad:?}");
    }
}
