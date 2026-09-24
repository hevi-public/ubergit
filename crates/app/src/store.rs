//! `RepoStore`: every repo's live summary, the selected repo's detail, the command
//! log, and the background work that keeps them fresh (watcher, poller, auto-fetch).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::{AppContext as _, Context, SharedString, Task};
use ubergit_core::detail::{self, RepoDetail};
use ubergit_core::watch::{self, RepoWatcher, WatchEvent};
use ubergit_core::{
    CmdKind, CommandRecord, CommandSink, Config, Git, GitError, GitOutput, RepoLocation, RepoSummary,
    discovery, ops, summary,
};

const LOG_CAPACITY: usize = 500;

pub struct RepoEntry {
    pub location: RepoLocation,
    pub summary: Option<RepoSummary>,
    /// Why the repo couldn't be read (e.g. `safe.directory`).
    pub error: Option<String>,
    pub fetch_error: Option<String>,
    /// Label of the operation running on this repo, e.g. "Fetching".
    pub busy: Option<SharedString>,
    refreshing: bool,
    refresh_again: bool,
}

impl RepoEntry {
    fn new(location: RepoLocation) -> Self {
        Self {
            location,
            summary: None,
            error: None,
            fetch_error: None,
            busy: None,
            refreshing: false,
            refresh_again: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.location.name
    }
}

pub struct Detail {
    pub root: PathBuf,
    pub data: RepoDetail,
    pub error: Option<String>,
}

pub struct RepoStore {
    pub workdir: PathBuf,
    pub config: Config,
    pub git: Git,
    pub repos: Vec<RepoEntry>,
    pub scanning: bool,
    /// Commands shown in the command log (mutations, network, and failures).
    pub log: VecDeque<CommandRecord>,
    pub selected: Option<PathBuf>,
    pub detail: Option<Detail>,
    /// Bumped whenever `detail` is replaced, so views can tell data changed.
    pub detail_generation: u64,
    detail_loading: bool,
    detail_again: bool,
    /// `(done, total)` while a fetch-all round is running.
    pub fetch_round: Option<(usize, usize)>,
    watcher: Option<RepoWatcher>,
    watch_tx: async_channel::Sender<WatchEvent>,
    _tasks: Vec<Task<()>>,
}

struct ChannelSink(async_channel::Sender<CommandRecord>);

impl CommandSink for ChannelSink {
    fn record(&self, record: CommandRecord) {
        let _ = self.0.try_send(record);
    }
}

impl RepoStore {
    pub fn new(workdir: PathBuf, config: Config, cx: &mut Context<Self>) -> Self {
        let (log_tx, log_rx) = async_channel::unbounded::<CommandRecord>();
        let (watch_tx, watch_rx) = async_channel::unbounded::<WatchEvent>();
        let git = Git::new(std::sync::Arc::new(ChannelSink(log_tx)));

        let log_task = cx.spawn(async move |this, cx| {
            while let Ok(record) = log_rx.recv().await {
                let keep = record.kind != CmdKind::Read || record.error.is_some();
                if keep && this.update(cx, |this, cx| this.push_log(record, cx)).is_err() {
                    break;
                }
            }
        });
        let watch_task = cx.spawn(async move |this, cx| {
            while let Ok(event) = watch_rx.recv().await {
                if this.update(cx, |this, cx| this.on_watch_event(event, cx)).is_err() {
                    break;
                }
            }
        });
        let poll_secs = config.poll_interval_secs.max(5);
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(poll_secs)).await;
                let alive = this.update(cx, |this, cx| {
                    this.refresh_all(cx);
                    this.load_detail(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let fetch_task = cx.spawn(async move |this, cx| {
            // Let the first summaries land before hitting the network.
            cx.background_executor().timer(Duration::from_secs(3)).await;
            loop {
                let Ok(interval) = this.update(cx, |this, cx| {
                    if this.config.auto_fetch {
                        this.fetch_all(cx);
                    }
                    this.config.fetch_interval_secs.max(30)
                }) else {
                    break;
                };
                cx.background_executor().timer(Duration::from_secs(interval)).await;
            }
        });

        let mut store = Self {
            workdir,
            config,
            git,
            repos: Vec::new(),
            scanning: false,
            log: VecDeque::new(),
            selected: None,
            detail: None,
            detail_generation: 0,
            detail_loading: false,
            detail_again: false,
            fetch_round: None,
            watcher: None,
            watch_tx,
            _tasks: vec![log_task, watch_task, poll_task, fetch_task],
        };
        store.scan(cx);
        store
    }

    pub fn index_of(&self, root: &Path) -> Option<usize> {
        self.repos.iter().position(|r| r.location.root == root)
    }

    pub fn entry(&self, root: &Path) -> Option<&RepoEntry> {
        self.index_of(root).map(|ix| &self.repos[ix])
    }

    pub fn selected_entry(&self) -> Option<&RepoEntry> {
        self.selected.as_deref().and_then(|root| self.entry(root))
    }

    /// Detail for the selected repo, if loaded.
    pub fn selected_detail(&self) -> Option<&RepoDetail> {
        let detail = self.detail.as_ref()?;
        (Some(detail.root.as_path()) == self.selected.as_deref()).then_some(&detail.data)
    }

    fn push_log(&mut self, record: CommandRecord, cx: &mut Context<Self>) {
        if self.log.len() == LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(record);
        cx.notify();
    }

    /// Rediscovers repos under the workdir, keeping known state for existing ones.
    pub fn scan(&mut self, cx: &mut Context<Self>) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        cx.notify();
        let git = self.git.clone();
        let workdir = self.workdir.clone();
        let max_depth = self.config.max_depth;
        let found = cx.background_spawn(async move { discovery::discover(&git, &workdir, max_depth).await });
        cx.spawn(async move |this, cx| {
            let found = found.await;
            this.update(cx, |this, cx| this.apply_scan(found, cx)).ok();
        })
        .detach();
    }

    fn apply_scan(
        &mut self,
        found: Vec<(discovery::Candidate, anyhow::Result<RepoLocation>)>,
        cx: &mut Context<Self>,
    ) {
        self.scanning = false;
        let mut old: Vec<RepoEntry> = std::mem::take(&mut self.repos);
        for (candidate, resolved) in found {
            let location = match &resolved {
                Ok(location) => location.clone(),
                Err(_) => RepoLocation {
                    name: discovery::display_name(&self.workdir, &candidate.root),
                    git_dir: candidate.root.join(".git"),
                    common_dir: candidate.root.join(".git"),
                    root: candidate.root.clone(),
                    bare: candidate.bare,
                },
            };
            let mut entry = match old.iter().position(|e| e.location.root == location.root) {
                Some(ix) => old.swap_remove(ix),
                None => RepoEntry::new(location.clone()),
            };
            entry.location = location;
            entry.error = resolved.err().map(|e| format!("{e:#}"));
            self.repos.push(entry);
        }
        self.repos.sort_by(|a, b| a.location.name.cmp(&b.location.name));

        let locations: Vec<RepoLocation> = self
            .repos
            .iter()
            .filter(|r| r.error.is_none())
            .map(|r| r.location.clone())
            .collect();
        let tx = self.watch_tx.clone();
        self.watcher = match watch::watch(&self.workdir, &locations, move |event| {
            let _ = tx.send_blocking(event);
        }) {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                log::error!("file watching unavailable: {err}");
                None
            }
        };
        self.refresh_all(cx);
        cx.notify();
    }

    fn on_watch_event(&mut self, event: WatchEvent, cx: &mut Context<Self>) {
        match event {
            WatchEvent::Rescan => self.scan(cx),
            WatchEvent::Changed(roots) => {
                for root in &roots {
                    self.refresh(root, cx);
                }
                if self.selected.as_ref().is_some_and(|s| roots.contains(s)) {
                    self.load_detail(cx);
                }
            }
        }
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self.repos.iter().map(|r| r.location.root.clone()).collect();
        for root in roots {
            self.refresh(&root, cx);
        }
    }

    /// Recomputes one repo's summary. Coalesces: at most one in flight per repo.
    pub fn refresh(&mut self, root: &Path, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(root) else { return };
        let entry = &mut self.repos[ix];
        if entry.busy.is_some() {
            return; // refreshed when the operation finishes
        }
        if entry.refreshing {
            entry.refresh_again = true;
            return;
        }
        entry.refreshing = true;
        let git = self.git.clone();
        let location = entry.location.clone();
        let root = root.to_path_buf();
        let task = cx.background_spawn(async move { summary::summarize(&git, &location).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Some(ix) = this.index_of(&root) else { return };
                let entry = &mut this.repos[ix];
                entry.refreshing = false;
                match result {
                    Ok(summary) => {
                        entry.summary = Some(summary);
                        entry.error = None;
                    }
                    Err(err) => entry.error = Some(format!("{err:#}")),
                }
                if std::mem::take(&mut entry.refresh_again) {
                    this.refresh(&root, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn select(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.selected == root {
            return;
        }
        self.selected = root;
        self.load_detail(cx);
        cx.notify();
    }

    /// (Re)loads the selected repo's panels. Coalesces like [`Self::refresh`].
    pub fn load_detail(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.selected.clone() else { return };
        if self.detail_loading {
            self.detail_again = true;
            return;
        }
        let Some(entry) = self.entry(&root) else { return };
        if entry.error.is_some() {
            return;
        }
        let location = entry.location.clone();
        let base = entry.summary.as_ref().and_then(|s| s.default_branch.clone());
        let has_commits = entry
            .summary
            .as_ref()
            .is_none_or(|s| !matches!(s.head, ubergit_core::Head::Unborn(_)));
        self.detail_loading = true;
        let git = self.git.clone();
        let task = cx.background_spawn(async move {
            detail::load(&git, &location, base.as_ref(), has_commits).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.detail_loading = false;
                let (data, error) = match result {
                    Ok(data) => (data, None),
                    Err(err) => (RepoDetail::default(), Some(format!("{err:#}"))),
                };
                this.detail = Some(Detail { root: root.clone(), data, error });
                this.detail_generation += 1;
                if std::mem::take(&mut this.detail_again) || this.selected.as_ref() != Some(&root) {
                    this.load_detail(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs a mutating operation on a repo, marking it busy and refreshing afterwards.
    pub fn run_op<T, Fut>(
        &mut self,
        root: &Path,
        label: &str,
        op: impl FnOnce(Git, RepoLocation) -> Fut,
        cx: &mut Context<Self>,
    ) -> Task<Result<T, GitError>>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, GitError>> + Send + 'static,
    {
        let Some(ix) = self.index_of(root) else {
            return Task::ready(Err(GitError::Failed {
                command: label.to_string(),
                code: None,
                stderr: format!("{} is no longer in the workdir", root.display()),
                stdout: String::new(),
            }));
        };
        let entry = &mut self.repos[ix];
        entry.busy = Some(SharedString::from(label.to_string()));
        let task = cx.background_spawn(op(self.git.clone(), entry.location.clone()));
        let root = root.to_path_buf();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(ix) = this.index_of(&root) {
                    this.repos[ix].busy = None;
                }
                this.refresh(&root, cx);
                if this.selected.as_ref() == Some(&root) {
                    this.load_detail(cx);
                }
                cx.notify();
            })
            .ok();
            result
        })
    }

    pub fn fetch(&mut self, root: &Path, cx: &mut Context<Self>) -> Task<Result<GitOutput, GitError>> {
        let task = self.run_op(root, "Fetching", |git, loc| async move { ops::fetch(&git, &loc).await }, cx);
        let root = root.to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, _| {
                if let Some(ix) = this.index_of(&root) {
                    this.repos[ix].fetch_error = result.as_ref().err().map(|e| e.to_string());
                }
            })
            .ok();
            result
        })
    }

    /// Fetches every repo with a remote, once per shared git dir.
    pub fn fetch_all(&mut self, cx: &mut Context<Self>) {
        if self.fetch_round.is_some() {
            return;
        }
        let roots = self.repos.iter().map(|r| r.location.root.clone()).collect();
        self.fetch_many(roots, cx);
    }

    /// Fetches the given repos that have a remote, once per shared git dir. Joins the
    /// running round, if any, so the bottom bar counts them all.
    pub fn fetch_many(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let targets: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| self.entry(root))
            .filter(|r| r.busy.is_none() && r.error.is_none())
            .filter(|r| r.summary.as_ref().is_some_and(RepoSummary::has_remote))
            .filter(|r| seen.insert(r.location.common_dir.clone()))
            .map(|r| r.location.root.clone())
            .collect();
        if targets.is_empty() {
            return;
        }
        self.fetch_round.get_or_insert((0, 0)).1 += targets.len();
        for root in targets {
            let task = self.fetch(&root, cx);
            cx.spawn(async move |this, cx| {
                task.await.ok();
                this.update(cx, |this, cx| {
                    if let Some((done, total)) = this.fetch_round.as_mut() {
                        *done += 1;
                        if *done >= *total {
                            this.fetch_round = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }
}
