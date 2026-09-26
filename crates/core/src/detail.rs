//! Loads everything the per-repo panels (Files, Branches, Commits, Stash...) show,
//! and the text for the main view.

use futures::future::join;

use crate::git::{CmdKind, Git, GitCommand, parse};
use crate::model::*;

/// Commits and reflog entries loaded per repo.
pub const LOG_LIMIT: usize = 300;
/// Lines of main-view output kept; the rest is dropped with a marker line.
pub const MAX_MAIN_LINES: usize = 20_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoDetail {
    pub files: Vec<FileEntry>,
    pub worktrees: Vec<Worktree>,
    pub submodules: Vec<Submodule>,
    pub branches: Vec<Branch>,
    pub remotes: Vec<Remote>,
    pub remote_branches: Vec<RemoteBranch>,
    pub tags: Vec<Tag>,
    pub commits: Vec<Commit>,
    pub reflog: Vec<ReflogEntry>,
    pub stashes: Vec<StashEntry>,
}

/// `has_commits` is false for an unborn HEAD, where log/reflog would only fail.
pub async fn load(
    git: &Git,
    repo: &RepoLocation,
    base: Option<&DefaultBranch>,
    has_commits: bool,
) -> anyhow::Result<RepoDetail> {
    let cwd = repo.root.as_path();
    let bare = repo.bare;

    let files = async {
        if bare {
            return Ok(Vec::new());
        }
        let out = git
            .read(cwd, ["status", "--porcelain=v2", "-z", "--untracked-files=all"])
            .await?;
        anyhow::Ok(parse::status_v2(&out.stdout).files)
    };
    let worktrees = async {
        let out = git.read(cwd, ["worktree", "list", "--porcelain", "-z"]).await?;
        anyhow::Ok(parse::worktrees(&out.stdout, &repo.root))
    };
    let submodules = async {
        if bare || !repo.root.join(".gitmodules").exists() {
            return Ok(Vec::new());
        }
        let out = git.read(cwd, ["submodule", "status"]).await?;
        anyhow::Ok(parse::submodules(&out.stdout_str()))
    };
    let branches = async {
        let format = format!("--format={}", parse::branch_format(base.map(|b| b.full_ref.as_str())));
        let out = git
            .read(cwd, ["for-each-ref", "--sort=-committerdate", &format, "refs/heads"])
            .await?;
        anyhow::Ok(parse::branches(&out.stdout_str(), Some(&repo.root)))
    };
    let remotes = async {
        let out = git.read(cwd, ["remote", "-v"]).await?;
        anyhow::Ok(parse::remotes(&out.stdout_str()))
    };
    let remote_branches = async {
        let format = format!("--format={}", parse::REMOTE_BRANCH_FORMAT);
        let out = git
            .read(cwd, ["for-each-ref", "--sort=refname", &format, "refs/remotes"])
            .await?;
        anyhow::Ok(parse::remote_branches(&out.stdout_str()))
    };
    let tags = async {
        let format = format!("--format={}", parse::TAG_FORMAT);
        let out = git
            .read(cwd, ["for-each-ref", "--sort=-creatordate", &format, "refs/tags"])
            .await?;
        anyhow::Ok(parse::tags(&out.stdout_str()))
    };
    let commits = async {
        if !has_commits {
            return Ok(Vec::new());
        }
        let format = format!("--format={}", parse::LOG_FORMAT);
        let limit = format!("-{LOG_LIMIT}");
        let out = git
            .read(cwd, ["log", "--no-show-signature", &format, &limit, "HEAD", "--"])
            .await?;
        anyhow::Ok(parse::log(&out.stdout_str()))
    };
    let reflog = async {
        if !has_commits {
            return Ok(Vec::new());
        }
        let format = format!("--format={}", parse::REFLOG_FORMAT);
        let limit = format!("-{LOG_LIMIT}");
        // A repo can have commits but no reflog (e.g. bare, or core.logAllRefUpdates off).
        let out = git.read(cwd, ["reflog", "show", &format, &limit, "HEAD", "--"]).await;
        anyhow::Ok(out.map(|o| parse::reflog(&o.stdout_str())).unwrap_or_default())
    };
    let stashes = async {
        if bare {
            return Ok(Vec::new());
        }
        let format = format!("--format={}", parse::STASH_FORMAT);
        let out = git.read(cwd, ["stash", "list", &format]).await?;
        anyhow::Ok(parse::stashes(&out.stdout_str()))
    };

    let (((files, worktrees), (submodules, branches)), ((remotes, remote_branches), (tags, (commits, (reflog, stashes))))) =
        join(
            join(join(files, worktrees), join(submodules, branches)),
            join(join(remotes, remote_branches), join(tags, join(commits, join(reflog, stashes)))),
        )
        .await;

    Ok(RepoDetail {
        files: files?,
        worktrees: worktrees?,
        submodules: submodules?,
        branches: branches?,
        remotes: remotes?,
        remote_branches: remote_branches?,
        tags: tags?,
        commits: commits?,
        reflog: reflog?,
        stashes: stashes?,
    })
}

const DIFF_FLAGS: &[&str] = &["--no-ext-diff", "--no-color", "--src-prefix=a/", "--dst-prefix=b/"];

/// Unstaged and staged diffs for one file, as lazygit shows them side by side. They're
/// the raw bytes, without textconv, so lines can be staged from them ([`crate::patch`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub unstaged: Option<Vec<u8>>,
    pub staged: Option<Vec<u8>>,
}

/// `file` as git sees it now, or `None` once it has no changes. The file list's copy can
/// be a moment old (e.g. just after staging part of it).
pub async fn file_status(git: &Git, repo: &RepoLocation, file: &FileEntry) -> anyhow::Result<Option<FileEntry>> {
    let mut args: Vec<String> = ["status", "--porcelain=v2", "-z", "--untracked-files=all", "--"]
        .map(String::from)
        .into();
    args.push(file.path.clone());
    args.extend(file.orig_path.clone());
    let out = git.read(&repo.root, args).await?;
    Ok(parse::status_v2(&out.stdout).files.into_iter().find(|f| f.path == file.path))
}

pub async fn file_diff(git: &Git, repo: &RepoLocation, file: &FileEntry) -> anyhow::Result<FileDiff> {
    let cwd = repo.root.as_path();
    let mut paths = vec![file.path.clone()];
    paths.extend(file.orig_path.clone());

    if file.kind == FileKind::Untracked {
        let mut args: Vec<String> = vec!["diff".into(), "--no-index".into(), "--no-textconv".into()];
        args.extend(DIFF_FLAGS.iter().map(|s| s.to_string()));
        args.extend(["--".into(), "/dev/null".into(), file.path.clone()]);
        let out = git
            .run(GitCommand::new(cwd, CmdKind::Read, args).ok_codes([1]))
            .await?;
        return Ok(FileDiff {
            unstaged: Some(out.stdout),
            staged: None,
        });
    }

    let diff = |cached: bool| {
        let mut args: Vec<String> = vec!["diff".into(), "--no-textconv".into()];
        args.extend(DIFF_FLAGS.iter().map(|s| s.to_string()));
        if cached {
            args.push("--cached".into());
            args.push("-M".into());
        }
        args.push("--".into());
        args.extend(paths.iter().cloned());
        async move { git.read(cwd, args).await.map(|o| o.stdout) }
    };
    let (unstaged, staged) = join(
        async {
            if file.has_unstaged() { Some(diff(false).await).transpose() } else { Ok(None) }
        },
        async {
            if file.has_staged() { Some(diff(true).await).transpose() } else { Ok(None) }
        },
    )
    .await;
    Ok(FileDiff {
        unstaged: unstaged?,
        staged: staged?,
    })
}

/// `git show` of a commit (or stash/tag): header, stat and patch.
pub async fn show(git: &Git, repo: &RepoLocation, rev: &str) -> anyhow::Result<String> {
    let mut args: Vec<&str> = vec!["show", "--no-show-signature", "--stat", "--patch", "--format=fuller"];
    args.extend(DIFF_FLAGS);
    args.extend([rev, "--"]);
    Ok(git.read(&repo.root, args).await?.stdout_str())
}

pub async fn stash_show(git: &Git, repo: &RepoLocation, index: usize) -> anyhow::Result<String> {
    let stash = format!("stash@{{{index}}}");
    let mut args: Vec<&str> = vec!["stash", "show", "--stat", "--patch", "--include-untracked"];
    args.extend(DIFF_FLAGS);
    args.push(&stash);
    Ok(git.read(&repo.root, args).await?.stdout_str())
}

/// Graph log of a ref, like lazygit's branch view.
pub async fn graph_log(git: &Git, repo: &RepoLocation, rev: &str) -> anyhow::Result<String> {
    let args = [
        "log",
        "--graph",
        "--no-show-signature",
        "--decorate",
        "--date=relative",
        "--format=%h %ad %an%d%n%s",
        "-200",
        rev,
        "--",
    ];
    Ok(git.read(&repo.root, args).await?.stdout_str())
}

/// Splits command output into display lines, capped at [`MAX_MAIN_LINES`].
pub fn to_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .take(MAX_MAIN_LINES)
        .map(|l| l.replace('\t', "    "))
        .collect();
    if text.lines().nth(MAX_MAIN_LINES).is_some() {
        lines.push(format!("… output truncated at {MAX_MAIN_LINES} lines"));
    }
    lines
}
