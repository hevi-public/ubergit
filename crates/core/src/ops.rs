//! Mutating git operations. All go through the CLI so hooks, signing and
//! credential helpers behave exactly as in the user's terminal.

use crate::git::{CmdKind, Git, GitCommand, GitError, GitOutput};
use crate::model::*;

type Result<T = GitOutput> = std::result::Result<T, GitError>;

fn path_args(file: &FileEntry) -> Vec<String> {
    let mut args = vec!["--".to_string(), file.path.clone()];
    args.extend(file.orig_path.clone());
    args
}

/// `space` in Files: stage if anything is unstaged, otherwise unstage.
pub async fn toggle_stage(git: &Git, repo: &RepoLocation, file: &FileEntry, unborn: bool) -> Result {
    if file.has_unstaged() {
        let mut args = vec!["add".to_string(), "-A".into()];
        args.extend(path_args(file));
        git.write(&repo.root, args).await
    } else {
        unstage(git, repo, file, unborn).await
    }
}

pub async fn unstage(git: &Git, repo: &RepoLocation, file: &FileEntry, unborn: bool) -> Result {
    let mut args: Vec<String> = if unborn {
        vec!["rm".into(), "--cached".into(), "-q".into(), "-r".into()]
    } else {
        vec!["reset".into(), "-q".into(), "HEAD".into()]
    };
    args.extend(path_args(file));
    git.write(&repo.root, args).await
}

/// `a` in Files: stage everything, or unstage everything if all is staged.
pub async fn toggle_stage_all(git: &Git, repo: &RepoLocation, files: &[FileEntry], unborn: bool) -> Result {
    if files.iter().any(FileEntry::has_unstaged) {
        git.write(&repo.root, ["add", "-A"]).await
    } else if unborn {
        git.write(&repo.root, ["rm", "--cached", "-q", "-r", "."]).await
    } else {
        git.write(&repo.root, ["reset", "-q"]).await
    }
}

/// Throws away all changes to a file, staged and unstaged.
pub async fn discard(git: &Git, repo: &RepoLocation, file: &FileEntry) -> Result {
    match file.kind {
        FileKind::Untracked => {
            git.write(&repo.root, ["clean", "-f", "-d", "--", file.path.as_str()]).await
        }
        _ if file.index == 'A' => {
            git.write(&repo.root, ["rm", "-f", "-q", "--", file.path.as_str()]).await
        }
        _ => {
            let mut args = vec![
                "restore".to_string(),
                "--staged".into(),
                "--worktree".into(),
                "--source=HEAD".into(),
            ];
            args.extend(path_args(file));
            git.write(&repo.root, args).await
        }
    }
}

/// What [`apply_patch`] does with a patch from [`crate::patch::Patch::build`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchAction {
    /// Adds the changes to the index.
    Stage,
    /// Takes them back out of the index.
    Unstage,
    /// Reverts them in the worktree.
    Discard,
}

/// `git apply` of a partial patch, reading it from stdin. Whitespace isn't checked: these
/// are the user's own changes moving between the worktree and the index.
pub async fn apply_patch(git: &Git, repo: &RepoLocation, patch: Vec<u8>, action: PatchAction) -> Result {
    let mut args = vec!["apply", "--whitespace=nowarn"];
    args.extend(match action {
        PatchAction::Stage => &["--cached"][..],
        PatchAction::Unstage => &["--cached", "--reverse"],
        PatchAction::Discard => &["--reverse"],
    });
    git.run(GitCommand::new(&repo.root, CmdKind::Write, args).stdin(patch)).await
}

pub async fn commit(git: &Git, repo: &RepoLocation, message: &str, amend: bool) -> Result {
    let mut args = vec!["commit", "--cleanup=strip", "-F", "-"];
    if amend {
        args.push("--amend");
    }
    git.run(GitCommand::new(&repo.root, CmdKind::Write, args).stdin(message.as_bytes()))
        .await
}

pub async fn last_commit_message(git: &Git, repo: &RepoLocation) -> Result<String> {
    let out = git.read(&repo.root, ["log", "-1", "--format=%B", "HEAD"]).await?;
    Ok(out.stdout_str().trim_end().to_string())
}

/// What [`stash`] puts away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StashKind {
    /// Every change, untracked files included, leaving a clean worktree.
    All,
    /// Every change, but staged changes also stay staged in the worktree.
    KeepIndex,
    /// Changes to tracked files; untracked files stay.
    Tracked,
    /// Only what's staged.
    Staged,
}

/// `git stash push`. Without a message git writes its own: `WIP on <branch>: <commit>`.
pub async fn stash(git: &Git, repo: &RepoLocation, kind: StashKind, message: Option<&str>) -> Result {
    let mut args = vec!["stash", "push"];
    args.extend(match kind {
        StashKind::All => &["--include-untracked"][..],
        StashKind::KeepIndex => &["--include-untracked", "--keep-index"],
        StashKind::Tracked => &[],
        StashKind::Staged => &["--staged"],
    });
    if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
        args.extend(["-m", message]);
    }
    git.write(&repo.root, args).await
}

/// Splits a stash subject into its branch and message: `On main: fix` → (`main`, `fix`),
/// `WIP on main: 1a2b3c4 subject` → (`main`, `1a2b3c4 subject`).
pub fn stash_subject_parts(subject: &str) -> Option<(&str, &str)> {
    let rest = subject.strip_prefix("On ").or_else(|| subject.strip_prefix("WIP on "))?;
    rest.split_once(": ")
}

/// Renames a stash. Git can't edit a stash message in place, so this drops the entry and
/// stores the same commit again with the new message; the stash moves to the top.
pub async fn stash_rename(git: &Git, repo: &RepoLocation, stash: &StashEntry, message: &str) -> Result {
    let selector = format!("stash@{{{}}}", stash.index);
    let oid = git.read(&repo.root, ["rev-parse", &selector]).await?.stdout_str().trim().to_string();
    // Keep git's `On <branch>: ` prefix so it reads like any other stash.
    let message = match stash_subject_parts(&stash.subject) {
        Some((branch, _)) => format!("On {branch}: {message}"),
        None => message.to_string(),
    };
    git.write(&repo.root, ["stash", "drop", "--quiet", &selector]).await?;
    git.write(&repo.root, ["stash", "store", "-m", &message, &oid])
        .await
        .map_err(|err| match err {
            // The dropped stash's commit still exists; say how to get it back.
            GitError::Failed { command, code, stderr, stdout } => GitError::Failed {
                command,
                code,
                stderr: format!("{stderr}\nThe stash was dropped but its commit is kept. Restore it with:\n  git stash store -m '{message}' {oid}"),
                stdout,
            },
            other => other,
        })
}

/// `git stash branch`: a new branch at the commit the stash was made on, with the stash
/// applied (and dropped if that worked).
pub async fn stash_branch(git: &Git, repo: &RepoLocation, index: usize, name: &str) -> Result {
    git.write(&repo.root, ["stash", "branch", name, &format!("stash@{{{index}}}")]).await
}

pub async fn stash_apply(git: &Git, repo: &RepoLocation, index: usize) -> Result {
    git.write(&repo.root, ["stash", "apply", &format!("stash@{{{index}}}")]).await
}

pub async fn stash_pop(git: &Git, repo: &RepoLocation, index: usize) -> Result {
    git.write(&repo.root, ["stash", "pop", &format!("stash@{{{index}}}")]).await
}

pub async fn stash_drop(git: &Git, repo: &RepoLocation, index: usize) -> Result {
    git.write(&repo.root, ["stash", "drop", &format!("stash@{{{index}}}")]).await
}

pub async fn checkout(git: &Git, repo: &RepoLocation, branch: &str) -> Result {
    git.write(&repo.root, ["checkout", branch]).await
}

pub async fn checkout_previous(git: &Git, repo: &RepoLocation) -> Result {
    git.write(&repo.root, ["checkout", "-"]).await
}

/// Checks out a remote branch as a local tracking branch (or the existing local one).
pub async fn checkout_remote(git: &Git, repo: &RepoLocation, remote_branch: &RemoteBranch) -> Result {
    let full = format!("{}/{}", remote_branch.remote, remote_branch.name);
    match git.write(&repo.root, ["checkout", "--track", &full]).await {
        Err(GitError::Failed { stderr, .. }) if stderr.contains("already exists") => {
            checkout(git, repo, &remote_branch.name).await
        }
        other => other,
    }
}

pub async fn new_branch(git: &Git, repo: &RepoLocation, name: &str, start: Option<&str>) -> Result {
    let mut args = vec!["checkout", "-b", name];
    args.extend(start);
    git.write(&repo.root, args).await
}

pub async fn delete_branch(git: &Git, repo: &RepoLocation, name: &str, force: bool) -> Result {
    let flag = if force { "-D" } else { "-d" };
    git.write(&repo.root, ["branch", flag, name]).await
}

pub fn is_not_fully_merged(err: &GitError) -> bool {
    matches!(err, GitError::Failed { stderr, .. } if stderr.contains("not fully merged"))
}

/// Fast-forwards `branch` to its (already fetched) upstream without network access.
pub async fn fast_forward(git: &Git, repo: &RepoLocation, branch: &Branch) -> Result {
    let Upstream::Tracking { name: upstream, .. } = &branch.upstream else {
        return Err(GitError::Failed {
            command: "fast-forward".into(),
            code: None,
            stderr: format!("{} has no upstream", branch.name),
            stdout: String::new(),
        });
    };
    if branch.is_head {
        git.write(&repo.root, ["merge", "--ff-only", upstream.as_str()]).await
    } else {
        let refspec = format!("refs/remotes/{upstream}:refs/heads/{}", branch.name);
        git.write(&repo.root, ["fetch", ".", &refspec]).await
    }
}

pub async fn set_upstream(git: &Git, repo: &RepoLocation, branch: &str, upstream: &str) -> Result {
    git.write(&repo.root, ["branch", &format!("--set-upstream-to={upstream}"), branch])
        .await
}

pub async fn fetch(git: &Git, repo: &RepoLocation) -> Result {
    git.network(
        &repo.root,
        [
            "fetch",
            "--all",
            "--quiet",
            "--no-auto-maintenance",
            "--recurse-submodules=no",
        ],
    )
    .await
}

pub async fn pull(git: &Git, repo: &RepoLocation) -> Result {
    git.network(&repo.root, ["pull", "--no-edit", "--recurse-submodules=no"])
        .await
}

/// Pushes the current branch; without an upstream, pushes to `remote` and sets it.
pub async fn push(git: &Git, repo: &RepoLocation, has_upstream: bool, remote: Option<&str>, force: bool) -> Result {
    let mut args = vec!["push"];
    if force {
        args.push("--force-with-lease");
    }
    if !has_upstream {
        args.extend(["--set-upstream", remote.unwrap_or("origin"), "HEAD"]);
    }
    git.network(&repo.root, args).await
}

pub fn is_push_rejected(err: &GitError) -> bool {
    matches!(err, GitError::Failed { stderr, .. }
        if stderr.contains("[rejected]") || stderr.contains("non-fast-forward") || stderr.contains("fetch first"))
}

/// Fast-forwards the checked-out branch to its upstream (used by "update all").
pub async fn fast_forward_head(git: &Git, repo: &RepoLocation) -> Result {
    git.write(&repo.root, ["merge", "--ff-only", "@{upstream}"]).await
}

/// Loose check for a branch name the user typed; git has the final say. Mainly keeps a
/// name from being read as an option or a revision expression.
pub fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains(|c: char| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
}

/// What [`switch_branch`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Switched {
    /// Checked out the existing local branch.
    Local,
    /// Created the local branch, tracking this remote branch (e.g. `origin/feat`).
    Tracking(String),
    /// No local or remote branch has that name; nothing changed.
    Missing,
}

/// Switches to branch `name`: the local branch if there is one, otherwise a new local
/// branch tracking `<remote>/<name>` (primary remote first). `git switch` only accepts
/// branches, so a name can never be taken for a path.
pub async fn switch_branch(git: &Git, repo: &RepoLocation, name: &str, remotes: &[String]) -> Result<Switched> {
    let primary = crate::summary::primary_remote(remotes, None);
    let mut ordered: Vec<&str> = primary.into_iter().collect();
    ordered.extend(remotes.iter().map(String::as_str).filter(|r| Some(*r) != primary));

    let local = format!("refs/heads/{name}");
    let remote_refs: Vec<String> = ordered.iter().map(|r| format!("refs/remotes/{r}/{name}")).collect();
    let mut args = vec!["for-each-ref".to_string(), "--format=%(refname)".into(), local.clone()];
    args.extend(remote_refs.iter().cloned());
    let out = git.read(&repo.root, args).await?.stdout_str();
    // A pattern also matches refs below it (`feat` matches `feat/x`), so compare exactly.
    let exists = |full: &str| out.lines().any(|line| line == full);

    if exists(&local) {
        git.write(&repo.root, ["switch", name]).await?;
        return Ok(Switched::Local);
    }
    for (remote, full) in ordered.iter().zip(&remote_refs) {
        if exists(full) {
            let start = format!("{remote}/{name}");
            git.write(&repo.root, ["switch", "--track", "-c", name, &start]).await?;
            return Ok(Switched::Tracking(start));
        }
    }
    Ok(Switched::Missing)
}

/// Splits a default branch into its local name and remote:
/// `refs/remotes/origin/main` → (`main`, `origin`), `refs/heads/main` → (`main`, none).
pub fn default_branch_parts(default: &DefaultBranch, remotes: &[String]) -> (String, Option<String>) {
    if let Some(name) = default.full_ref.strip_prefix("refs/heads/") {
        return (name.to_string(), None);
    }
    let short = default.full_ref.strip_prefix("refs/remotes/").unwrap_or(&default.short);
    // Remote names may contain '/', so take the longest remote that prefixes it.
    let remote = remotes
        .iter()
        .filter(|r| short.starts_with(&format!("{r}/")))
        .max_by_key(|r| r.len());
    match remote {
        Some(remote) => (short[remote.len() + 1..].to_string(), Some(remote.clone())),
        None => match short.split_once('/') {
            Some((remote, name)) => (name.to_string(), Some(remote.to_string())),
            None => (short.to_string(), None),
        },
    }
}

/// What [`switch_to_default`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultSwitch {
    /// Local name of the default branch, e.g. `main`.
    pub branch: String,
    /// `None` when the default branch was already checked out.
    pub switched: Option<Switched>,
    /// The branch's upstream, e.g. `origin/main`, if it has one.
    pub upstream: Option<String>,
    /// Divergence from the upstream before fast-forwarding.
    pub ahead: u32,
    pub behind: u32,
    pub fast_forwarded: bool,
}

/// Checks out the default branch (creating it to track the remote one if needed), then
/// fast-forwards it to its upstream. Offline: uses whatever the last fetch brought in.
/// `current` is the checked-out branch, if any.
pub async fn switch_to_default(
    git: &Git,
    repo: &RepoLocation,
    default: &DefaultBranch,
    remotes: &[String],
    current: Option<&str>,
) -> Result<DefaultSwitch> {
    let (branch, remote) = default_branch_parts(default, remotes);
    let mut result = DefaultSwitch {
        branch: branch.clone(),
        switched: None,
        upstream: None,
        ahead: 0,
        behind: 0,
        fast_forwarded: false,
    };
    if current != Some(branch.as_str()) {
        let remotes: Vec<String> = remote.into_iter().collect();
        let switched = switch_branch(git, repo, &branch, &remotes).await?;
        let missing = switched == Switched::Missing;
        result.switched = Some(switched);
        if missing {
            return Ok(result);
        }
    }
    let upstream = git
        .run(
            GitCommand::new(
                &repo.root,
                CmdKind::Read,
                ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
            )
            .ok_codes([128]), // no upstream configured, or it's gone
        )
        .await?
        .stdout_str()
        .trim()
        .to_string();
    if upstream.is_empty() {
        return Ok(result);
    }
    let counts = git
        .read(&repo.root, ["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .await?;
    let (ahead, behind) = crate::git::parse::left_right(&counts.stdout_str()).unwrap_or((0, 0));
    result.upstream = Some(upstream);
    result.ahead = ahead;
    result.behind = behind;
    if behind > 0 && ahead == 0 {
        fast_forward_head(git, repo).await?;
        result.fast_forwarded = true;
    }
    Ok(result)
}
