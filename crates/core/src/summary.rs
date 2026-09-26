//! Computes a [`RepoSummary`]: the at-a-glance state shown for every repo.

use std::path::Path;
use std::time::SystemTime;

use crate::git::{CmdKind, Git, GitCommand, parse};
use crate::github::remote_branch;
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
    let refs = read_refs(git, &repo.root, &remotes, status.upstream.as_deref(), status.branch.as_deref()).await?;
    let default_branch = refs.default_branch;
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
        head_oid: status.oid.clone(),
        upstream: status.upstream(),
        upstream_oid: refs.upstream_oid,
        remote_branch_oid: refs.remote_branch_oid,
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
    if let Some((remote, _)) = upstream.and_then(|upstream| split_upstream(remotes, upstream)) {
        return Some(remote);
    }
    remotes
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| remotes.first())
        .map(String::as_str)
}

/// The remote and branch name of an upstream on a remote, from status's short name for
/// it: `origin/feat`, or `remotes/origin/feat` when a local branch is named `origin/feat`
/// too. `None` for a local upstream.
pub fn split_upstream<'a, 'u>(remotes: &'a [String], upstream: &'u str) -> Option<(&'a str, &'u str)> {
    let split = |name: &'u str| {
        // Remote names may contain '/', so pick the longest remote that prefixes it.
        remotes
            .iter()
            .filter_map(|remote| Some((remote.as_str(), name.strip_prefix(remote.as_str())?.strip_prefix('/')?)))
            .max_by_key(|(remote, _)| remote.len())
    };
    // Git reads `remotes/origin/feat` as `refs/remotes/origin/feat` before it would try a
    // remote named `remotes`, so the stripped name wins. That still guesses wrong for a
    // remote named `remotes` whose branch starts with another remote's name, like
    // `origin/feat` on it: only `branch.<name>.remote` would tell.
    upstream.strip_prefix("remotes/").and_then(split).or_else(|| split(upstream))
}

/// What [`read_refs`] finds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refs {
    pub default_branch: Option<DefaultBranch>,
    pub upstream_oid: Option<String>,
    pub remote_branch_oid: Option<String>,
}

/// Resolves the default branch: `<remote>/HEAD`, else `<remote>/main`, else
/// `<remote>/master`; with no remote, local `main` then `master`. Also reads the commit
/// of `upstream` (status's short name for it), and of the remote branch a PR for `branch`
/// would come from, in the same `for-each-ref`, as this runs on every refresh of every repo.
pub async fn read_refs(
    git: &Git,
    cwd: &Path,
    remotes: &[String],
    upstream: Option<&str>,
    branch: Option<&str>,
) -> anyhow::Result<Refs> {
    let patterns: Vec<String> = match primary_remote(remotes, upstream) {
        Some(remote) => ["HEAD", "main", "master"]
            .iter()
            .map(|b| format!("refs/remotes/{remote}/{b}"))
            .collect(),
        None => vec!["refs/heads/main".into(), "refs/heads/master".into()],
    };
    let candidates = upstream.map(upstream_candidates).unwrap_or_default();
    let remote_branch: Option<String> = branch
        .and_then(|branch| remote_branch(upstream, remotes, branch))
        .map(|(remote, branch)| format!("refs/remotes/{remote}/{branch}"));
    let mut args = vec![
        "for-each-ref".to_string(),
        "--format=%(refname)%1f%(symref)%1f%(objectname)%1e".to_string(),
    ];
    args.extend(patterns.iter().chain(&candidates).chain(&remote_branch).cloned());
    let out = git.read(cwd, args).await?;
    let text = out.stdout_str();
    let refs: Vec<(String, String, String)> = parse::records(&text)
        .filter_map(|f| match f[..] {
            [name, symref, oid] => Some((name.to_string(), symref.to_string(), oid.to_string())),
            _ => None,
        })
        .collect();
    Ok(Refs {
        default_branch: pick_default_branch(&patterns, &refs),
        upstream_oid: pick_upstream_oid(&candidates, &refs),
        remote_branch_oid: pick_upstream_oid(remote_branch.as_slice(), &refs),
    })
}

/// `refs` are `(refname, symref-target, oid)` triples that exist, `patterns` in priority order.
pub fn pick_default_branch(patterns: &[String], refs: &[(String, String, String)]) -> Option<DefaultBranch> {
    let (name, symref, _) = first_ref(patterns, refs)?;
    let full_ref = if symref.is_empty() { name } else { symref };
    Some(DefaultBranch {
        short: short_ref(full_ref),
        full_ref: full_ref.clone(),
    })
}

/// The refs status's short name for an upstream may stand for, in the order git tries
/// them (tags left out: an upstream is a branch). Status shortens a name only as far as it
/// still resolves to the same ref, so the first of these that exists is the upstream:
/// `origin/feat` is normally `refs/remotes/origin/feat`, a local upstream `main` is
/// `refs/heads/main`, and beside a local branch named `origin/feat` the remote one is
/// called `remotes/origin/feat`.
pub fn upstream_candidates(short: &str) -> Vec<String> {
    ["refs/", "refs/heads/", "refs/remotes/"]
        .iter()
        .map(|prefix| format!("{prefix}{short}"))
        .collect()
}

/// The commit of the first of `candidates` (e.g. [`upstream_candidates`]) in `refs`, as in
/// [`pick_default_branch`].
pub fn pick_upstream_oid(candidates: &[String], refs: &[(String, String, String)]) -> Option<String> {
    first_ref(candidates, refs).map(|(_, _, oid)| oid.clone())
}

/// The first of `names` in `refs`, by exact name: `refs` holds what every pattern listed,
/// and `for-each-ref` also lists the refs under a pattern, e.g. `refs/heads/origin/feat/x`
/// for `refs/heads/origin/feat`.
fn first_ref<'a>(names: &[String], refs: &'a [(String, String, String)]) -> Option<&'a (String, String, String)> {
    names
        .iter()
        .find_map(|wanted| refs.iter().find(|(name, ..)| name == wanted))
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
        // Beside a local branch named `fork/feat`, status calls the upstream this.
        assert_eq!(primary_remote(&remotes, Some("remotes/fork/feat")), Some("fork"));
        assert_eq!(primary_remote(&remotes, Some("main")), Some("origin"));
        assert_eq!(primary_remote(&remotes, None), Some("origin"));
        assert_eq!(primary_remote(&["up".to_string()], None), Some("up"));
        assert_eq!(primary_remote(&[], None), None);
    }

    #[test]
    fn splits_an_upstream_into_remote_and_branch() {
        let remotes = vec!["origin".to_string(), "team".to_string(), "team/x".to_string()];
        let split = |upstream| split_upstream(&remotes, upstream);
        assert_eq!(split("origin/feat/a"), Some(("origin", "feat/a")));
        assert_eq!(split("team/x/fix"), Some(("team/x", "fix")));
        assert_eq!(split("team/fix"), Some(("team", "fix")));
        assert_eq!(split("remotes/origin/feat"), Some(("origin", "feat")));
        assert_eq!(split("remotes/team/x/fix"), Some(("team/x", "fix")));
        // Local upstreams.
        assert_eq!(split("main"), None);
        assert_eq!(split("remotes/main"), None);
        assert_eq!(split("originals/x"), None);
        // A remote really named `remotes`.
        let odd = vec!["remotes".to_string(), "origin".to_string()];
        assert_eq!(split_upstream(&odd, "remotes/x"), Some(("remotes", "x")));
        assert_eq!(split_upstream(&odd, "remotes/origin/x"), Some(("origin", "x")));
    }

    #[test]
    fn default_branch_prefers_symbolic_head() {
        let patterns: Vec<String> = ["HEAD", "main", "master"]
            .iter()
            .map(|b| format!("refs/remotes/origin/{b}"))
            .collect();
        let refs = vec![
            ("refs/remotes/origin/HEAD".into(), "refs/remotes/origin/develop".into(), "d1".into()),
            ("refs/remotes/origin/main".into(), String::new(), "m1".into()),
        ];
        let picked = pick_default_branch(&patterns, &refs).unwrap();
        assert_eq!(picked.short, "origin/develop");
        let picked = pick_default_branch(&patterns, &refs[1..]).unwrap();
        assert_eq!(picked.full_ref, "refs/remotes/origin/main");
        assert_eq!(pick_default_branch(&patterns, &[]), None);
    }

    #[test]
    fn upstream_oid_is_picked_by_exact_name() {
        let refs: Vec<(String, String, String)> = [
            ("refs/heads/main", "", "local-main"),
            ("refs/heads/origin/feat/x", "", "local-feat-x"),
            ("refs/heads/origin/fix", "", "local-fix"),
            ("refs/remotes/origin/HEAD", "refs/remotes/origin/main", "main"),
            ("refs/remotes/origin/feat", "", "feat"),
            ("refs/remotes/origin/fix", "", "fix"),
            ("refs/remotes/origin/main", "", "main"),
            ("refs/remotes/origin/old/x", "", "old-x"),
        ]
        .iter()
        .map(|(name, symref, oid)| (name.to_string(), symref.to_string(), oid.to_string()))
        .collect();
        let oid = |short: &str| pick_upstream_oid(&upstream_candidates(short), &refs);

        // On main tracking origin/main, one ref serves both lookups.
        assert_eq!(oid("origin/main").as_deref(), Some("main"));
        let patterns: Vec<String> = ["HEAD", "main", "master"]
            .iter()
            .map(|b| format!("refs/remotes/origin/{b}"))
            .collect();
        assert_eq!(pick_default_branch(&patterns, &refs).unwrap().short, "origin/main");
        // `refs/heads/origin/feat` is tried first and lists `refs/heads/origin/feat/x`.
        assert_eq!(oid("origin/feat").as_deref(), Some("feat"));
        // A gone upstream: only a branch under its name is left.
        assert_eq!(oid("origin/old"), None);
        // Beside a local `origin/fix`, status calls the remote one `remotes/origin/fix`,
        // and `origin/fix` is a local upstream.
        assert_eq!(oid("remotes/origin/fix").as_deref(), Some("fix"));
        assert_eq!(oid("origin/fix").as_deref(), Some("local-fix"));
        assert_eq!(oid("main").as_deref(), Some("local-main"));
    }
}
