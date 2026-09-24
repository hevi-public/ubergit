//! Live change detection: one recursive FSEvents watch on the workdir, events
//! routed to repos by path and coalesced per repo.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecursiveMode, Watcher as _};

use crate::discovery::SKIP_DIRS;
use crate::model::RepoLocation;

/// Quiet period after the last event before a repo is refreshed.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// Longest a repo waits under a continuous stream of events.
pub const MAX_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// These repo roots changed.
    Changed(Vec<PathBuf>),
    /// Something appeared or disappeared at the top of the workdir: rediscover.
    Rescan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Ignore,
    Repos(Vec<PathBuf>),
    Rescan,
}

enum Target {
    WorkTree(usize),
    /// A git dir. `own` are repos whose per-worktree state (HEAD, index...) lives here;
    /// `sharing` also includes linked worktrees that share its refs and config.
    GitDir { own: Vec<usize>, sharing: Vec<usize> },
}

/// Entries of a common git dir that all of its worktrees share.
const SHARED_GIT_PATHS: &[&str] = &["refs", "packed-refs", "config", "reftable", "shallow", "info"];

/// Maps a changed path to the repos it affects.
pub struct Router {
    workdir: PathBuf,
    roots: Vec<PathBuf>,
    /// Sorted longest-first so the most specific prefix wins.
    prefixes: Vec<(PathBuf, Target)>,
    ignores: Vec<Gitignore>,
    global_ignore: Gitignore,
}

/// Paths inside a git dir that never affect what we display.
const IGNORED_GIT_PATHS: &[&str] = &[
    "objects",
    "logs",
    "hooks",
    "lfs",
    "modules",
    "fsmonitor--daemon",
    "worktrees",
    "COMMIT_EDITMSG",
    "gc.log",
    "gc.pid",
];

impl Router {
    pub fn new(workdir: &Path, repos: &[RepoLocation]) -> Self {
        let mut git_dirs: HashMap<PathBuf, (Vec<usize>, Vec<usize>)> = HashMap::new();
        let mut prefixes = Vec::new();
        for (ix, repo) in repos.iter().enumerate() {
            if !repo.bare {
                prefixes.push((repo.root.clone(), Target::WorkTree(ix)));
            }
            let (own, sharing) = git_dirs.entry(repo.git_dir.clone()).or_default();
            own.push(ix);
            sharing.push(ix);
            if repo.common_dir != repo.git_dir {
                git_dirs.entry(repo.common_dir.clone()).or_default().1.push(ix);
            }
        }
        prefixes.extend(
            git_dirs
                .into_iter()
                .map(|(dir, (own, sharing))| (dir, Target::GitDir { own, sharing })),
        );
        prefixes.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));

        let ignores = repos
            .iter()
            .map(|repo| {
                let mut builder = GitignoreBuilder::new(&repo.root);
                builder.add(repo.root.join(".gitignore"));
                builder.add(repo.common_dir.join("info/exclude"));
                builder.build().unwrap_or_else(|_| Gitignore::empty())
            })
            .collect();

        Self {
            workdir: workdir.to_path_buf(),
            roots: repos.iter().map(|r| r.root.clone()).collect(),
            prefixes,
            ignores,
            global_ignore: Gitignore::global().0,
        }
    }

    pub fn route(&self, path: &Path) -> Route {
        let Some((prefix, target)) = self.prefixes.iter().find(|(p, _)| path.starts_with(p)) else {
            let top_level = path.parent() == Some(self.workdir.as_path());
            let git_marker = path.file_name().is_some_and(|n| n == ".git");
            return if top_level || git_marker { Route::Rescan } else { Route::Ignore };
        };
        let rel = path.strip_prefix(prefix).unwrap_or(path);
        match target {
            Target::GitDir { own, sharing } => {
                if is_ignored_git_path(rel) {
                    return Route::Ignore;
                }
                let shared = rel
                    .components()
                    .next()
                    .is_some_and(|c| SHARED_GIT_PATHS.iter().any(|p| c.as_os_str() == *p));
                let ixs = if shared { sharing } else { own };
                if ixs.is_empty() {
                    Route::Ignore
                } else {
                    Route::Repos(ixs.iter().map(|&ix| self.roots[ix].clone()).collect())
                }
            }
            Target::WorkTree(ix) => {
                if rel.as_os_str().is_empty() {
                    return Route::Ignore;
                }
                let skipped = rel.components().any(|c| match c {
                    Component::Normal(name) => {
                        name == ".git" || SKIP_DIRS.iter().any(|s| name == *s)
                    }
                    _ => false,
                });
                if skipped || self.is_gitignored(*ix, rel) {
                    Route::Ignore
                } else {
                    Route::Repos(vec![self.roots[*ix].clone()])
                }
            }
        }
    }

    fn is_gitignored(&self, ix: usize, rel: &Path) -> bool {
        // Events don't say whether the path was a directory; directory-only
        // patterns like `build/` still match through the parent check.
        self.ignores[ix].matched_path_or_any_parents(rel, false).is_ignore()
            || self.global_ignore.matched_path_or_any_parents(rel, false).is_ignore()
    }
}

fn is_ignored_git_path(rel: &Path) -> bool {
    let Some(Component::Normal(first)) = rel.components().next() else {
        return false;
    };
    let name = rel.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    IGNORED_GIT_PATHS.iter().any(|p| first == *p) || name.ends_with(".lock")
}

/// Keeps the watch alive; dropping it stops watching.
pub struct RepoWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Starts watching `workdir`. `on_event` is called from a background thread.
pub fn watch(
    workdir: &Path,
    repos: &[RepoLocation],
    on_event: impl Fn(WatchEvent) + Send + 'static,
) -> notify::Result<RepoWatcher> {
    let router = Router::new(workdir, repos);
    let all_roots: Vec<PathBuf> = repos.iter().map(|r| r.root.clone()).collect();
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })?;
    watcher.watch(workdir, RecursiveMode::Recursive)?;
    // Git dirs outside the workdir (e.g. worktrees of a repo elsewhere) need their own watch.
    for repo in repos {
        for dir in [&repo.git_dir, &repo.common_dir] {
            if !dir.starts_with(workdir) {
                let _ = watcher.watch(dir, RecursiveMode::Recursive);
            }
        }
    }

    std::thread::Builder::new()
        .name("ubergit-watch".into())
        .spawn(move || {
            // root -> (first event, last event)
            let mut pending: HashMap<PathBuf, (Instant, Instant)> = HashMap::new();
            let mut rescan: Option<Instant> = None;
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ok(event)) => {
                        let now = Instant::now();
                        let mut mark = |root: PathBuf| {
                            pending.entry(root).and_modify(|(_, last)| *last = now).or_insert((now, now));
                        };
                        if event.need_rescan() {
                            all_roots.iter().cloned().for_each(&mut mark);
                            rescan.get_or_insert(now);
                        }
                        for path in &event.paths {
                            match router.route(path) {
                                Route::Ignore => {}
                                Route::Repos(roots) => roots.into_iter().for_each(&mut mark),
                                Route::Rescan => {
                                    rescan.get_or_insert(now);
                                }
                            }
                        }
                    }
                    Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }

                let now = Instant::now();
                let due: Vec<PathBuf> = pending
                    .iter()
                    .filter(|(_, (first, last))| now - *last >= DEBOUNCE || now - *first >= MAX_DELAY)
                    .map(|(root, _)| root.clone())
                    .collect();
                if !due.is_empty() {
                    for root in &due {
                        pending.remove(root);
                    }
                    on_event(WatchEvent::Changed(due));
                }
                if rescan.is_some_and(|at| now - at >= Duration::from_secs(1)) {
                    rescan = None;
                    on_event(WatchEvent::Rescan);
                }
            }
        })
        .expect("spawn watcher thread");

    Ok(RepoWatcher { _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(root: &str, git_dir: &str, common_dir: &str) -> RepoLocation {
        RepoLocation {
            root: root.into(),
            name: root.rsplit('/').next().unwrap().into(),
            git_dir: git_dir.into(),
            common_dir: common_dir.into(),
            bare: false,
        }
    }

    #[test]
    fn routes_paths_to_repos() {
        let repos = vec![
            repo("/w/a", "/w/a/.git", "/w/a/.git"),
            repo("/w/b", "/w/a/.git/worktrees/b", "/w/a/.git"),
        ];
        let router = Router::new(Path::new("/w"), &repos);
        let r = |p: &str| router.route(Path::new(p));
        let a = || PathBuf::from("/w/a");
        let b = || PathBuf::from("/w/b");

        assert_eq!(r("/w/a/src/main.rs"), Route::Repos(vec![a()]));
        assert_eq!(r("/w/a/node_modules/x/y.js"), Route::Ignore);
        assert_eq!(r("/w/a/.git/index"), Route::Repos(vec![a()]));
        assert_eq!(r("/w/a/.git/index.lock"), Route::Ignore);
        assert_eq!(r("/w/a/.git/objects/ab/cdef"), Route::Ignore);
        // Shared refs affect the main checkout and its linked worktree.
        assert_eq!(r("/w/a/.git/refs/heads/main"), Route::Repos(vec![a(), b()]));
        // The linked worktree's own HEAD only affects it.
        assert_eq!(r("/w/a/.git/worktrees/b/HEAD"), Route::Repos(vec![b()]));
        assert_eq!(r("/w/b/file.txt"), Route::Repos(vec![b()]));
        assert_eq!(r("/w/new-service"), Route::Rescan);
        assert_eq!(r("/w/group/new/.git"), Route::Rescan);
        assert_eq!(r("/w/group/notes.txt"), Route::Ignore);
    }
}
