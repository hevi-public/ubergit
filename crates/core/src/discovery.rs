//! Finds git repositories under a workdir.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git::{Git, parse};
use crate::model::RepoLocation;

/// Directories never worth descending into when looking for repos.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    ".build",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
    ".next",
    "Pods",
    "DerivedData",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub root: PathBuf,
    pub bare: bool,
}

/// Walks `workdir` up to `max_depth` levels and returns directories that look like
/// repositories, sorted by path. Does not descend into a repo once found, so submodules
/// and nested repos are not listed separately.
pub fn find_candidates(workdir: &Path, max_depth: usize) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut walker = walkdir::WalkDir::new(workdir)
        .follow_links(false)
        .max_depth(max_depth)
        .sort_by_file_name()
        .into_iter();

    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        if entry.depth() > 0 {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                walker.skip_current_dir();
                continue;
            }
        }
        if path.join(".git").exists() {
            found.push(Candidate {
                root: path.to_path_buf(),
                bare: false,
            });
            walker.skip_current_dir();
        } else if looks_bare(path) {
            found.push(Candidate {
                root: path.to_path_buf(),
                bare: true,
            });
            walker.skip_current_dir();
        }
    }
    found
}

fn looks_bare(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

/// Asks git to confirm a candidate and resolve its git dirs.
pub async fn resolve(git: &Git, workdir: &Path, candidate: &Candidate) -> anyhow::Result<RepoLocation> {
    let out = git
        .read(
            &candidate.root,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
                "--is-bare-repository",
            ],
        )
        .await?;
    let text = out.stdout_str();
    let mut lines = text.lines();
    let (Some(git_dir), Some(common_dir), Some(bare)) = (lines.next(), lines.next(), lines.next())
    else {
        anyhow::bail!("unexpected rev-parse output: {text:?}");
    };
    Ok(RepoLocation {
        name: display_name(workdir, &candidate.root),
        root: candidate.root.clone(),
        git_dir: PathBuf::from(git_dir),
        common_dir: PathBuf::from(common_dir),
        bare: bare.trim() == "true",
    })
}

pub fn display_name(workdir: &Path, root: &Path) -> String {
    match root.strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        _ => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
    }
}

/// Discovers and resolves all repositories under `workdir`, plus the linked worktrees of
/// the ones found, wherever those live. Candidates git refuses (e.g. `safe.directory`)
/// are returned as errors alongside their path.
pub async fn discover(
    git: &Git,
    workdir: &Path,
    max_depth: usize,
) -> Vec<(Candidate, anyhow::Result<RepoLocation>)> {
    let mut found = resolve_all(git, workdir, find_candidates(workdir, max_depth)).await;
    let worktrees = linked_worktrees(git, workdir, &found).await;
    found.extend(resolve_all(git, workdir, worktrees).await);
    found
}

async fn resolve_all(
    git: &Git,
    workdir: &Path,
    candidates: Vec<Candidate>,
) -> Vec<(Candidate, anyhow::Result<RepoLocation>)> {
    let futures = candidates.into_iter().map(|candidate| async move {
        let resolved = resolve(git, workdir, &candidate).await;
        (candidate, resolved)
    });
    futures::future::join_all(futures).await
}

/// Linked worktrees of the main checkouts in `found` that the walk didn't list: those in
/// hidden directories (Claude Code keeps them in `.claude/worktrees/`), too deep, or
/// outside the workdir. Worktrees whose directory is gone are left out.
async fn linked_worktrees(
    git: &Git,
    workdir: &Path,
    found: &[(Candidate, anyhow::Result<RepoLocation>)],
) -> Vec<Candidate> {
    // git lists a worktree by the path it was added with, which may be spelled
    // differently (a symlink, `/tmp` for `/private/tmp`) from the walk's.
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let workdir_canonical = canonical(workdir);
    let mut known: HashSet<PathBuf> = found.iter().map(|(c, _)| canonical(&c.root)).collect();
    let mains = found
        .iter()
        .filter_map(|(_, loc)| loc.as_ref().ok())
        .filter(|loc| !loc.bare && !loc.is_linked_worktree());
    let lists = futures::future::join_all(mains.map(|loc| async move {
        match git.read(&loc.root, ["worktree", "list", "--porcelain", "-z"]).await {
            Ok(out) => parse::worktrees(&out.stdout, &loc.root),
            Err(_) => Vec::new(),
        }
    }))
    .await;
    let mut worktrees = Vec::new();
    for wt in lists.into_iter().flatten() {
        if wt.bare || wt.prunable || wt.is_current || !wt.path.is_dir() {
            continue;
        }
        let path = canonical(&wt.path);
        if !known.insert(path.clone()) {
            continue;
        }
        // Spelled like the walk's paths, so it's named relative to the workdir.
        let root = match path.strip_prefix(&workdir_canonical) {
            Ok(rel) => workdir.join(rel),
            Err(_) => path,
        };
        worktrees.push(Candidate { root, bare: false });
    }
    worktrees.sort_by(|a, b| a.root.cmp(&b.root));
    worktrees
}

/// Each repo's main checkout, as an index into `repos`: the non-linked repo sharing its
/// common git dir. `None` for main checkouts, and for linked worktrees whose main
/// checkout isn't listed.
pub fn main_checkouts(repos: &[&RepoLocation]) -> Vec<Option<usize>> {
    let mains: HashMap<&Path, usize> = repos
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.bare && !r.is_linked_worktree())
        .map(|(ix, r)| (r.common_dir.as_path(), ix))
        .collect();
    repos
        .iter()
        .map(|r| {
            if r.is_linked_worktree() {
                mains.get(r.common_dir.as_path()).copied()
            } else {
                None
            }
        })
        .collect()
}
