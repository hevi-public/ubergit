use std::path::PathBuf;
use std::time::SystemTime;

/// A repository found under the workdir, with paths resolved once at discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoLocation {
    /// Working tree root (or the git dir itself for bare repos).
    pub root: PathBuf,
    /// Display name: path relative to the workdir, `/`-separated.
    pub name: String,
    /// Per-worktree git dir (HEAD, index, FETCH_HEAD, rebase state live here).
    pub git_dir: PathBuf,
    /// Shared git dir (refs, objects, config). Equal to `git_dir` unless a linked worktree.
    pub common_dir: PathBuf,
    pub bare: bool,
}

impl RepoLocation {
    pub fn is_linked_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    /// Detached at the given (full) commit id.
    Detached(String),
    /// A branch with no commits yet.
    Unborn(String),
}

impl Head {
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Head::Branch(name) | Head::Unborn(name) => Some(name),
            Head::Detached(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upstream {
    None,
    Tracking { name: String, ahead: u32, behind: u32 },
    /// Configured upstream whose remote branch no longer exists.
    Gone { name: String },
}

/// Divergence of HEAD from the repo's default branch (e.g. `origin/main`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseDivergence {
    /// Short ref name, e.g. `origin/main`.
    pub base: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    /// Distinct changed files.
    pub files: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
}

impl ChangeCounts {
    pub fn is_clean(&self) -> bool {
        *self == ChangeCounts::default()
    }

    /// Staged, unstaged or conflicted changes to tracked files. Untracked files don't
    /// count: they stay put across checkouts, and git refuses if one would be overwritten.
    pub fn has_tracked_changes(&self) -> bool {
        self.staged + self.unstaged + self.conflicted > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoOp {
    /// `branch` is the branch being rebased (HEAD is detached meanwhile).
    Rebasing {
        step: Option<(u32, u32)>,
        branch: Option<String>,
    },
    Merging,
    CherryPicking,
    Reverting,
    Bisecting,
    ApplyingMailbox,
}

impl RepoOp {
    pub fn label(&self) -> String {
        match self {
            RepoOp::Rebasing { step: Some((n, total)), .. } => format!("rebasing {n}/{total}"),
            RepoOp::Rebasing { step: None, .. } => "rebasing".into(),
            RepoOp::Merging => "merging".into(),
            RepoOp::CherryPicking => "cherry-picking".into(),
            RepoOp::Reverting => "reverting".into(),
            RepoOp::Bisecting => "bisecting".into(),
            RepoOp::ApplyingMailbox => "applying".into(),
        }
    }
}

/// The branch a repo's work is measured against, e.g. `origin/main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultBranch {
    /// Full ref, e.g. `refs/remotes/origin/main` or `refs/heads/main`.
    pub full_ref: String,
    /// Display name, e.g. `origin/main`.
    pub short: String,
}

/// Everything the Repos panel and overview need about one repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSummary {
    pub head: Head,
    /// Full id of the HEAD commit, `None` while unborn. A merged PR only shows while its
    /// head is still this commit.
    pub head_oid: Option<String>,
    pub upstream: Upstream,
    /// The commit `@{upstream}` points to. A push, or a fetch that brings the branch new
    /// commits, moves it, so it tells when to ask for the branch's PR again. For a local
    /// upstream, the local branch's tip. `None` without an upstream, when it's gone, and
    /// in bare repos.
    pub upstream_oid: Option<String>,
    /// The commit of the branch a PR for HEAD's branch would come from, on its remote
    /// ([`crate::github::remote_branch`]). Usually the upstream's, but a branch pushed
    /// without `-u` has no upstream and still has `<remote>/<branch>`, which a push moves.
    /// `None` when detached, without remotes, or before the branch is pushed.
    pub remote_branch_oid: Option<String>,
    pub base: Option<BaseDivergence>,
    pub changes: ChangeCounts,
    pub stash_count: u32,
    pub op: Option<RepoOp>,
    pub last_fetch: Option<SystemTime>,
    pub remotes: Vec<String>,
    pub default_branch: Option<DefaultBranch>,
    pub shallow: bool,
}

impl RepoSummary {
    pub fn has_remote(&self) -> bool {
        !self.remotes.is_empty()
    }

    /// True when nothing needs the user's attention.
    pub fn is_settled(&self) -> bool {
        self.changes.is_clean()
            && self.op.is_none()
            && matches!(
                self.upstream,
                Upstream::Tracking { ahead: 0, behind: 0, .. }
            )
    }
}

/// One entry of `git status --porcelain=v2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// Source path for renames/copies.
    pub orig_path: Option<String>,
    /// Index status (`X`), `.` when unchanged, `?` for untracked.
    pub index: char,
    /// Worktree status (`Y`), `.` when unchanged, `?` for untracked.
    pub worktree: char,
    pub kind: FileKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Tracked,
    Renamed,
    Unmerged,
    Untracked,
}

impl FileEntry {
    pub fn has_staged(&self) -> bool {
        matches!(self.kind, FileKind::Tracked | FileKind::Renamed) && self.index != '.'
    }

    pub fn has_unstaged(&self) -> bool {
        match self.kind {
            FileKind::Untracked | FileKind::Unmerged => true,
            FileKind::Tracked | FileKind::Renamed => self.worktree != '.',
        }
    }

    /// Two-letter lazygit-style short status, e.g. `M `, ` M`, `??`, `UU`.
    pub fn short_status(&self) -> String {
        let x = if self.index == '.' { ' ' } else { self.index };
        let y = if self.worktree == '.' { ' ' } else { self.worktree };
        format!("{x}{y}")
    }
}

/// Parsed output of `git status --porcelain=v2 --branch --show-stash -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusReport {
    /// `None` when HEAD is unborn (`(initial)`).
    pub oid: Option<String>,
    /// `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// `(ahead, behind)`; absent when there is no upstream or it is gone.
    pub ahead_behind: Option<(u32, u32)>,
    pub stash_count: u32,
    pub files: Vec<FileEntry>,
}

impl StatusReport {
    pub fn change_counts(&self) -> ChangeCounts {
        let mut counts = ChangeCounts {
            files: self.files.len() as u32,
            ..Default::default()
        };
        for file in &self.files {
            match file.kind {
                FileKind::Untracked => counts.untracked += 1,
                FileKind::Unmerged => counts.conflicted += 1,
                FileKind::Tracked | FileKind::Renamed => {
                    if file.index != '.' {
                        counts.staged += 1;
                    }
                    if file.worktree != '.' {
                        counts.unstaged += 1;
                    }
                }
            }
        }
        counts
    }

    pub fn head(&self) -> Head {
        match (&self.branch, &self.oid) {
            (Some(branch), Some(_)) => Head::Branch(branch.clone()),
            (Some(branch), None) => Head::Unborn(branch.clone()),
            (None, oid) => Head::Detached(oid.clone().unwrap_or_default()),
        }
    }

    pub fn upstream(&self) -> Upstream {
        match (&self.upstream, self.ahead_behind) {
            (None, _) => Upstream::None,
            (Some(name), Some((ahead, behind))) => Upstream::Tracking {
                name: name.clone(),
                ahead,
                behind,
            },
            (Some(name), None) => Upstream::Gone { name: name.clone() },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub is_head: bool,
    pub short_oid: String,
    pub upstream: Upstream,
    /// Divergence from the repo's default branch, when known.
    pub base: Option<(u32, u32)>,
    pub committed: Option<SystemTime>,
    pub subject: String,
    /// Set when the branch is checked out in another worktree.
    pub worktree: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    /// Name without the remote prefix.
    pub name: String,
    pub short_oid: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub short_oid: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub oid: String,
    pub short_oid: String,
    pub author: String,
    pub time: Option<SystemTime>,
    pub subject: String,
    /// Decorations like `HEAD -> main, origin/main, tag: v1`.
    pub refs: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflogEntry {
    pub short_oid: String,
    pub selector: String,
    pub subject: String,
    pub time: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashEntry {
    pub index: usize,
    pub subject: String,
    pub time: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    /// Its directory or its git dir is gone: `git worktree prune` would drop it.
    pub prunable: bool,
    pub is_current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submodule {
    pub path: String,
    pub short_oid: String,
    /// `' '` in sync, `+` checked out at a different commit, `-` not initialised, `U` conflicts.
    pub state: char,
}

/// The pull request for a branch, as GitHub reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    /// `None` when the base branch doesn't require reviews.
    pub review: Option<ReviewDecision>,
    /// Checks on the PR's head commit; `None` when it has none.
    pub checks: Option<Checks>,
    pub reviewers: Vec<Reviewer>,
    /// The branch it merges into.
    pub base_ref: String,
    /// `owner/name` of the repo the PR is on. For a branch pushed to a fork that is
    /// usually the fork's parent, not the remote's repo.
    pub base_repo: String,
    /// The PR's head commit on GitHub, to tell whether local commits are pushed.
    pub head_oid: String,
}

/// Closed, unmerged PRs are never reported: the branch counts as having none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// CI on a PR's head commit: its check runs and commit statuses together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checks {
    pub state: ChecksState,
    /// Names of the checks that failed, in GitHub's order.
    pub failing: Vec<String>,
    /// All checks. `failing` and `pending` only look at the first 50.
    pub total: u32,
    /// Checks still queued or running.
    pub pending: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

/// A user or team asked to review, or who has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    /// Login, or a team's slug.
    pub name: String,
    pub state: ReviewerState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewerState {
    /// Asked to review and hasn't since, or was asked again after reviewing.
    Requested,
    Approved,
    ChangesRequested,
}
