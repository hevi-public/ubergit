//! `RepoStore`: every repo's live summary, the selected repo's detail, the command
//! log, and the background work that keeps them fresh (watcher, poller, auto-fetch,
//! PR lookups).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::{AppContext as _, Context, SharedString, Task};
use ubergit_core::detail::{self, RepoDetail};
use ubergit_core::pr_status::{self, Answer, GhStatus, Lookups, Next, PrQueue, PrView, RepoPr, Request};
use ubergit_core::watch::{self, RepoWatcher, WatchEvent};
use ubergit_core::{
    CmdKind, CommandRecord, CommandSink, Config, Gh, Git, GitError, GitOutput, RepoLocation, RepoSummary,
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
    /// The checked-out branch's pull request. Show it through [`RepoStore::pr_view`],
    /// which hides an answer for another branch.
    pub pr: RepoPr,
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
            pr: RepoPr::default(),
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
    /// Runs gh for PR lookups, logging to the command log like `git`.
    pub gh: Gh,
    /// Whether gh can be asked; the same for every repo.
    pub gh_status: GhStatus,
    pr_queue: PrQueue,
    /// Calls [`Self::pump_prs`] again when debounced repos are due.
    pr_wake: Option<Task<()>>,
    /// When every repo's PR was last queued, by the timer or `R`.
    last_pr_round: Instant,
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
        let sink = std::sync::Arc::new(ChannelSink(log_tx));
        let git = Git::new(sink.clone());
        let gh = Gh::new(sink);

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
        let mut tasks = vec![log_task, watch_task, poll_task, fetch_task];
        // An API read, not a fetch: `auto_fetch` and `--no-fetch` don't turn it off.
        let gh_status = if config.github_status {
            tasks.push(cx.spawn(async move |this, cx| {
                loop {
                    let Ok(wait) = this.update(cx, |this, cx| this.pr_round_if_due(cx)) else {
                        break;
                    };
                    cx.background_executor().timer(wait).await;
                }
            }));
            tasks.push(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(pr_status::PENDING_INTERVAL).await;
                    if this.update(cx, |this, cx| this.recheck_pending_prs(cx)).is_err() {
                        break;
                    }
                }
            }));
            GhStatus::Unchecked
        } else {
            GhStatus::Disabled
        };

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
            gh,
            gh_status,
            pr_queue: PrQueue::default(),
            pr_wake: None,
            // The first summaries queue every repo, which makes the first round.
            last_pr_round: Instant::now(),
            watcher: None,
            watch_tx,
            _tasks: tasks,
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
        // Remote URLs are read again when next needed, in case one was changed.
        for entry in &mut self.repos {
            entry.pr.remotes.clear();
        }
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
                let again = std::mem::take(&mut entry.refresh_again);
                this.queue_pr_if_changed(ix, cx);
                if again {
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

    /// What to show for a repo's PR: only an answer for the checked-out branch.
    pub fn pr_view<'a>(&'a self, entry: &'a RepoEntry) -> PrView<'a> {
        // A repo that can't be read may still have an old summary.
        let summary = entry.summary.as_ref().filter(|_| entry.error.is_none());
        pr_status::view(&self.gh_status, summary, entry.location.bare, &entry.pr)
    }

    /// `R`: asks gh again whether it's installed and where it's logged in, then looks up
    /// every repo's PR. Not again while that's under way, or just after a round.
    pub fn refresh_prs(&mut self, cx: &mut Context<Self>) {
        if self.gh_status == GhStatus::Disabled {
            return;
        }
        let roots = self.repos.iter().map(|r| r.location.root.clone());
        if self.pr_queue.refresh(roots, self.last_pr_round.elapsed()) {
            self.last_pr_round = Instant::now();
            self.pump_prs(cx);
        }
    }

    /// Looks up one repo's PR now rather than when it's next due, for `G` on a repo whose PR
    /// isn't known.
    pub fn look_up_pr(&mut self, root: &Path, cx: &mut Context<Self>) {
        self.pr_queue.now([root.to_path_buf()]);
        self.pump_prs(cx);
    }

    /// Queues every repo's PR when a timer round is due; returns the time until the next.
    fn pr_round_if_due(&mut self, cx: &mut Context<Self>) -> Duration {
        let interval = self.config.pr_interval();
        let elapsed = self.last_pr_round.elapsed();
        // Not due when `R` made a round meanwhile. A second of slack for the timer.
        if elapsed + Duration::from_secs(1) < interval {
            return interval - elapsed;
        }
        self.last_pr_round = Instant::now();
        let roots: Vec<PathBuf> = self.repos.iter().map(|r| r.location.root.clone()).collect();
        self.pr_queue.round(roots, &self.gh_status);
        self.pump_prs(cx);
        interval
    }

    /// Queues the PRs whose checks were still running.
    fn recheck_pending_prs(&mut self, cx: &mut Context<Self>) {
        let now = SystemTime::now();
        let roots: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|entry| pr_status::recheck_due(&self.pr_view(entry), now))
            .map(|entry| entry.location.root.clone())
            .collect();
        if !roots.is_empty() {
            self.pr_queue.now(roots);
            self.pump_prs(cx);
        }
    }

    /// Queues the repo's PR, debounced, when a new summary changed what it's for.
    fn queue_pr_if_changed(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.gh_status == GhStatus::Disabled {
            return;
        }
        let entry = &mut self.repos[ix];
        let Some(summary) = entry.summary.as_ref().filter(|_| entry.error.is_none()) else {
            return;
        };
        let Ok(key) = pr_status::pr_key(summary, entry.location.bare) else {
            return;
        };
        if entry.pr.update_key(&key, summary.head_oid.as_deref()) {
            self.pr_queue.changed(entry.location.root.clone(), Instant::now());
            self.pump_prs(cx);
        }
    }

    /// Starts the next PR lookup, or wakes up when debounced repos are due.
    fn pump_prs(&mut self, cx: &mut Context<Self>) {
        match self.pr_queue.next(&self.gh_status, Instant::now()) {
            Next::Idle => {}
            Next::Wait(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                self.pr_wake = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    this.update(cx, |this, cx| this.pump_prs(cx)).ok();
                }));
            }
            Next::Start { repos, check_hosts } => self.start_pr_lookup(repos, check_hosts, cx),
        }
    }

    fn start_pr_lookup(&mut self, roots: Vec<PathBuf>, check_hosts: bool, cx: &mut Context<Self>) {
        // What each repo is for now, which may have changed since it was queued.
        let mut requests: Vec<Request> = Vec::with_capacity(roots.len());
        for root in roots {
            let Some(ix) = self.index_of(&root) else { continue };
            let entry = &mut self.repos[ix];
            let readable = entry.summary.as_ref().filter(|_| entry.error.is_none());
            let found = readable.and_then(|s| Some((s, pr_status::pr_key(s, entry.location.bare).ok()?)));
            let Some((summary, key)) = found else {
                // Detached meanwhile, say: going back to the branch queues it again.
                entry.pr.unqueue();
                continue;
            };
            requests.push(Request {
                root,
                // A full round reads the URL again, in case `git remote set-url` changed it.
                // Until it answers, the one read before still shows.
                remote: if check_hosts { None } else { entry.pr.remotes.get(&key.remote).cloned() },
                local_oid: summary.head_oid.clone(),
                key,
            });
        }
        if requests.is_empty() && !check_hosts {
            self.pr_queue.finished();
            return;
        }
        log::debug!(
            "looking up {} PRs{}",
            requests.len(),
            if check_hosts { ", checking gh's logins first" } else { "" }
        );
        let hosts = self.gh_status.hosts().map(<[String]>::to_vec);
        let (git, gh, workdir) = (self.git.clone(), self.gh.clone(), self.workdir.clone());
        let task = cx.background_spawn(async move {
            pr_status::look_up(&git, &gh, &workdir, check_hosts, hosts, requests).await
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            this.update(cx, |this, cx| {
                this.apply_pr_lookups(found);
                this.pr_queue.finished();
                this.pump_prs(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_pr_lookups(&mut self, found: Lookups) {
        if let Some(status) = found.gh {
            if status != self.gh_status {
                log::debug!("gh: {status:?}");
            }
            // gh itself is the problem, not the repos.
            if matches!(status, GhStatus::NotInstalled | GhStatus::NotLoggedIn) {
                for entry in &mut self.repos {
                    entry.pr.error = None;
                }
            }
            self.gh_status = status;
        }
        let now = SystemTime::now();
        for answered in found.repos {
            let Some(ix) = self.index_of(&answered.root) else {
                continue;
            };
            let entry = &mut self.repos[ix];
            log::debug!("PR of {} ({}): {}", entry.name(), answered.key.branch, describe(&answered.answer));
            entry.pr.record(answered, now);
        }
    }
}

/// A lookup's answer, for the debug log.
fn describe(answer: &Answer) -> String {
    match answer {
        Answer::Lookup(lookup) => match &lookup.pr {
            Some(pr) => {
                let checks = pr.checks.as_ref().map(|c| format!(", checks {:?}", c.state)).unwrap_or_default();
                format!("#{} {:?}{checks}", pr.number, pr.state)
            }
            None if lookup.create_url.is_some() => "no PR (can open one)".into(),
            None => "no PR".into(),
        },
        Answer::NotOnGitHub => "not on GitHub".into(),
        Answer::Failed(message) => format!("failed: {message}"),
        Answer::NotAsked => "not asked".into(),
    }
}
