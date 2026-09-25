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

/// Output of a git command in a fixture repo.
fn output(name: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo(name).root)
        .args(args)
        .envs(ISOLATED)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} in {name}");
    String::from_utf8(out.stdout).unwrap()
}

fn stash_subjects(name: &str) -> Vec<String> {
    output(name, &["stash", "list", "--format=%gs"]).lines().map(str::to_string).collect()
}

fn stash(name: &str, kind: ops::StashKind, message: Option<&str>) {
    block_on(ops::stash(&git(), repo(name), kind, message)).unwrap();
}

#[test]
fn stashes_staged_changes_with_a_message_then_everything_else() {
    // `dirty`: README.md staged, app.txt modified, untracked.txt new.
    stash("dirty", ops::StashKind::Staged, Some("only staged"));
    assert_eq!(stash_subjects("dirty"), ["On main: only staged"]);
    let c = summary("dirty").changes;
    assert_eq!((c.staged, c.unstaged, c.untracked), (0, 1, 1));

    stash("dirty", ops::StashKind::All, Some("  "));
    assert!(summary("dirty").changes.is_clean());
    let subjects = stash_subjects("dirty");
    assert_eq!(subjects.len(), 2);
    assert!(subjects[0].starts_with("WIP on main: "), "{subjects:?}");
}

#[test]
fn tracked_stash_leaves_untracked_files_and_renames_keep_the_branch() {
    // `detached` has no changes; add one tracked and one untracked.
    std::fs::write(repo("detached").root.join("README.md"), "changed\n").unwrap();
    std::fs::write(repo("detached").root.join("new.txt"), "new\n").unwrap();
    stash("detached", ops::StashKind::Tracked, Some("first"));
    let c = summary("detached").changes;
    assert_eq!((c.unstaged, c.untracked), (0, 1));
    assert_eq!(stash_subjects("detached"), ["On (no branch): first"]);

    let entry = StashEntry { index: 0, subject: "On (no branch): first".into(), time: None };
    let oid = output("detached", &["rev-parse", "stash@{0}"]);
    block_on(ops::stash_rename(&git(), repo("detached"), &entry, "second")).unwrap();
    assert_eq!(stash_subjects("detached"), ["On (no branch): second"]);
    assert_eq!(output("detached", &["rev-parse", "stash@{0}"]), oid, "same stash commit");
}

#[test]
fn renaming_moves_the_stash_to_the_top() {
    // `stashed` has two stashes; rename the older one.
    let before = stash_subjects("stashed");
    let oid = output("stashed", &["rev-parse", "stash@{1}"]);
    let entry = StashEntry { index: 1, subject: before[1].clone(), time: None };
    block_on(ops::stash_rename(&git(), repo("stashed"), &entry, "the first one")).unwrap();
    assert_eq!(stash_subjects("stashed"), ["On main: the first one", before[0].as_str()]);
    assert_eq!(output("stashed", &["rev-parse", "stash@{0}"]), oid);
}

#[test]
fn keep_index_stash_and_branch_from_stash() {
    // `ahead`: stage one change, keep it in the index while stashing.
    std::fs::write(repo("ahead").root.join("app.txt"), "staged\n").unwrap();
    setup("ahead", &["add", "app.txt"]);
    stash("ahead", ops::StashKind::KeepIndex, Some("kept"));
    assert_eq!(summary("ahead").changes.staged, 1, "staged change stays");
    setup("ahead", &["reset", "--hard", "-q"]);

    block_on(ops::stash_branch(&git(), repo("ahead"), 0, "from-stash")).unwrap();
    let after = summary("ahead");
    assert_eq!(after.head, Head::Branch("from-stash".into()));
    assert_eq!(after.changes.staged, 1, "stash applied with its index");
    assert_eq!(after.stash_count, 0, "stash dropped after applying");
}

#[test]
fn stash_subject_parts() {
    assert_eq!(ops::stash_subject_parts("On main: fix it"), Some(("main", "fix it")));
    assert_eq!(ops::stash_subject_parts("WIP on feat/x: 1a2b3c4 msg"), Some(("feat/x", "1a2b3c4 msg")));
    assert_eq!(ops::stash_subject_parts("On (no branch): x"), Some(("(no branch)", "x")));
    assert_eq!(ops::stash_subject_parts("custom"), None);
}
