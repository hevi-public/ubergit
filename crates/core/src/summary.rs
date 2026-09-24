//! Computes a [`RepoSummary`]: the at-a-glance state shown for every repo.

use std::path::Path;
use std::time::SystemTime;

use crate::git::{CmdKind, Git, GitCommand, parse};
use crate::model::*;

pub async fn summarize(git: &Git, repo: &RepoLocation) -> anyhow::Result<RepoSummary> {
    let status = if repo.bare {
        let branch = git.read(&repo.root, ["symbolic-ref", "-q", "--short", "HEAD"]).await;
        // Exit code 1 just means there are no commits yet.
        let oid = git
            .run(
                GitCommand::new(&repo.root, CmdKind::Read, ["rev-parse", "-q", "--verify", "HEAD^{commit}"])
                    .ok_codes([1]),
            )
            .await?;
        let oid = oid.stdout_str().trim().to_string();
        StatusReport {
            branch: branch.ok().map(|o| o.stdout_str().trim().to_string()),
            oid: (!oid.is_empty()).then_some(oid),
            ..Default::default()
        }
    } else {
        let out = git
            .read(
                &repo.root,
                [
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "--show-stash",
                    "-z",
                    "--untracked-files=normal",
                ],
            )
            .await?;
        parse::status_v2(&out.stdout)
    };

    let remotes = list_remotes(git, &repo.root).await?;
    let default_branch = default_branch(git, &repo.root, &remotes, status.upstream.as_deref()).await?;
    let head = status.head();

    let base = match (&default_branch, &head) {
        (Some(default), Head::Branch(_) | Head::Detached(_)) => {
            let range = format!("HEAD...{}", default.full_ref);
            let out = git
                .read(&repo.root, ["rev-list", "--left-right", "--count", &range])
                .await?;
            parse::left_right(&out.stdout_str()).map(|(ahead, behind)| BaseDivergence {
                base: default.short.clone(),
                ahead,
                behind,
            })
        }
        _ => None,
    };

    Ok(RepoSummary {
        upstream: status.upstream(),
        changes: status.change_counts(),
        stash_count: status.stash_count,
        head,
        base,
        op: repo_op(&repo.git_dir),
        last_fetch: last_fetch(repo),
        remotes,
        default_branch,
        shallow: repo.common_dir.join("shallow").exists(),
    })
}

pub async fn list_remotes(git: &Git, cwd: &Path) -> anyhow::Result<Vec<String>> {
    let out = git.read(cwd, ["remote"]).await?;
    Ok(out.stdout_str().lines().map(str::to_string).collect())
}

/// Which remote the repo's default branch lives on: the upstream's remote, else
/// `origin`, else the only/first remote.
pub fn primary_remote<'a>(remotes: &'a [String], upstream: Option<&str>) -> Option<&'a str> {
    if let Some(upstream) = upstream {
        // Remote names may contain '/', so pick the longest remote that prefixes it.
        let from_upstream = remotes
            .iter()
            .filter(|r| upstream.starts_with(&format!("{r}/")))
            .max_by_key(|r| r.len());
        if let Some(remote) = from_upstream {
            return Some(remote);
        }
    }
    remotes
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| remotes.first())
        .map(String::as_str)
}

/// Resolves the default branch: `<remote>/HEAD`, else `<remote>/main`, else
/// `<remote>/master`; with no remote, local `main` then `master`.
pub async fn default_branch(
    git: &Git,
    cwd: &Path,
    remotes: &[String],
    upstream: Option<&str>,
) -> anyhow::Result<Option<DefaultBranch>> {
    let patterns: Vec<String> = match primary_remote(remotes, upstream) {
        Some(remote) => ["HEAD", "main", "master"]
            .iter()
            .map(|b| format!("refs/remotes/{remote}/{b}"))
            .collect(),
        None => vec!["refs/heads/main".into(), "refs/heads/master".into()],
    };
    let mut args = vec![
        "for-each-ref".to_string(),
        "--format=%(refname)%1f%(symref)%1e".to_string(),
    ];
    args.extend(patterns.iter().cloned());
    let out = git.read(cwd, args).await?;
    let text = out.stdout_str();
    let refs: Vec<(String, String)> = parse::records(&text)
        .filter_map(|f| match f[..] {
            [name, symref] => Some((name.to_string(), symref.to_string())),
            _ => None,
        })
        .collect();
    Ok(pick_default_branch(&patterns, &refs))
}

/// `refs` are `(refname, symref-target)` pairs that exist, `patterns` in priority order.
pub fn pick_default_branch(patterns: &[String], refs: &[(String, String)]) -> Option<DefaultBranch> {
    for pattern in patterns {
        let Some((name, symref)) = refs.iter().find(|(name, _)| name == pattern) else {
            continue;
        };
        let full_ref = if symref.is_empty() { name } else { symref };
        return Some(DefaultBranch {
            short: short_ref(full_ref),
            full_ref: full_ref.clone(),
        });
    }
    None
}

pub fn short_ref(full_ref: &str) -> String {
    full_ref
        .strip_prefix("refs/remotes/")
        .or_else(|| full_ref.strip_prefix("refs/heads/"))
        .unwrap_or(full_ref)
        .to_string()
}

/// Detects an in-progress rebase/merge/etc. from marker files in the git dir.
pub fn repo_op(git_dir: &Path) -> Option<RepoOp> {
    let read = |dir: &Path, file: &str| {
        std::fs::read_to_string(dir.join(file))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let number = |dir: &Path, file: &str| read(dir, file)?.parse::<u32>().ok();
    let branch = |dir: &Path| {
        read(dir, "head-name").map(|name| name.trim_start_matches("refs/heads/").to_string())
    };

    let rebase_merge = git_dir.join("rebase-merge");
    if rebase_merge.is_dir() {
        return Some(RepoOp::Rebasing {
            step: number(&rebase_merge, "msgnum").zip(number(&rebase_merge, "end")),
            branch: branch(&rebase_merge),
        });
    }
    let rebase_apply = git_dir.join("rebase-apply");
    if rebase_apply.is_dir() {
        if rebase_apply.join("applying").exists() {
            return Some(RepoOp::ApplyingMailbox);
        }
        return Some(RepoOp::Rebasing {
            step: number(&rebase_apply, "next").zip(number(&rebase_apply, "last")),
            branch: branch(&rebase_apply),
        });
    }
    if git_dir.join("MERGE_HEAD").exists() {
        return Some(RepoOp::Merging);
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Some(RepoOp::CherryPicking);
    }
    if git_dir.join("REVERT_HEAD").exists() {
        return Some(RepoOp::Reverting);
    }
    if git_dir.join("BISECT_LOG").exists() {
        return Some(RepoOp::Bisecting);
    }
    None
}

/// Most recent FETCH_HEAD write for this worktree or the shared repo.
pub fn last_fetch(repo: &RepoLocation) -> Option<SystemTime> {
    [&repo.git_dir, &repo.common_dir]
        .iter()
        .filter_map(|dir| std::fs::metadata(dir.join("FETCH_HEAD")).ok()?.modified().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_remote_prefers_upstream_then_origin() {
        let remotes = vec!["fork".to_string(), "origin".to_string(), "team/x".to_string()];
        assert_eq!(primary_remote(&remotes, Some("fork/feat")), Some("fork"));
        assert_eq!(primary_remote(&remotes, Some("team/x/feat")), Some("team/x"));
        assert_eq!(primary_remote(&remotes, None), Some("origin"));
        assert_eq!(primary_remote(&["up".to_string()], None), Some("up"));
        assert_eq!(primary_remote(&[], None), None);
    }

    #[test]
    fn default_branch_prefers_symbolic_head() {
        let patterns: Vec<String> = ["HEAD", "main", "master"]
            .iter()
            .map(|b| format!("refs/remotes/origin/{b}"))
            .collect();
        let refs = vec![
            ("refs/remotes/origin/HEAD".into(), "refs/remotes/origin/develop".into()),
            ("refs/remotes/origin/main".into(), String::new()),
        ];
        let picked = pick_default_branch(&patterns, &refs).unwrap();
        assert_eq!(picked.short, "origin/develop");
        let picked = pick_default_branch(&patterns, &refs[1..]).unwrap();
        assert_eq!(picked.full_ref, "refs/remotes/origin/main");
        assert_eq!(pick_default_branch(&patterns, &[]), None);
    }
}
