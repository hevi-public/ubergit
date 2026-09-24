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

pub async fn stash_all(git: &Git, repo: &RepoLocation) -> Result {
    git.write(&repo.root, ["stash", "push", "--include-untracked"]).await
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
