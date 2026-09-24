//! Marking repos in the Repos panel, and running one operation on many repos at once.
//!
//! Actions started from the Repos panel run on every marked repo, or on the selected
//! one when nothing is marked. Each repo is checked first (busy, mid-rebase, dirty...)
//! and skipped with a reason rather than half-done; the rest run in parallel and a
//! popup fills in each repo's outcome as it lands.

use std::future::Future;
use std::path::PathBuf;

use gpui_kit::{prelude::*, *};
use ubergit_core::ops::{self, DefaultSwitch, Switched};
use ubergit_core::summary::primary_remote;
use ubergit_core::{Git, GitError, Head, RepoLocation, RepoSummary, Upstream};

use crate::keymap::*;
use crate::store::RepoEntry;
use crate::workspace::{Dialog, Panel, View, Workspace};

#[derive(Clone, Debug)]
pub enum Outcome {
    Pending,
    Done(String),
    Skipped(String),
    /// Full error text; the popup shows its [`headline`].
    Failed(String),
}

pub struct BatchRow {
    pub name: String,
    pub outcome: Outcome,
}

impl Workspace {
    // ---- marks ------------------------------------------------------------------------------

    /// Marked repos in Repos-panel order, when the action starts from the Repos panel.
    pub(crate) fn marked_targets(&self, cx: &App) -> Option<Vec<PathBuf>> {
        if self.focused != Panel::Repos || self.marked.is_empty() {
            return None;
        }
        let store = self.store.read(cx);
        Some(
            store
                .repos
                .iter()
                .map(|r| &r.location.root)
                .filter(|root| self.marked.contains(*root))
                .cloned()
                .collect(),
        )
    }

    /// Marked repos, else the selected one.
    fn targets(&self, cx: &App) -> Vec<PathBuf> {
        self.marked_targets(cx)
            .unwrap_or_else(|| self.selected_root(cx).into_iter().collect())
    }

    /// "billing" or "3 repos", for prompt titles.
    fn describe_targets(&self, targets: &[PathBuf], cx: &App) -> String {
        match targets {
            [root] => self.store.read(cx).entry(root).map(|e| e.name().to_string()).unwrap_or_default(),
            _ => format!("{} repos", targets.len()),
        }
    }

    pub fn toggle_mark(&mut self, _: &ToggleMark, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.selected_root(cx)
            && !self.marked.remove(&root)
        {
            self.marked.insert(root);
        }
        cx.notify();
    }

    /// Marks every listed (filtered) repo, or unmarks them if all are marked already.
    pub fn toggle_mark_all(&mut self, _: &ToggleMarkAll, _: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let listed: Vec<PathBuf> = self
            .visible(View::Repos, store)
            .into_iter()
            .map(|ix| store.repos[ix].location.root.clone())
            .collect();
        if listed.iter().all(|root| self.marked.contains(root)) {
            for root in &listed {
                self.marked.remove(root);
            }
        } else {
            self.marked.extend(listed);
        }
        cx.notify();
    }

    /// Cmd-click on a row of the Repos panel.
    pub(crate) fn toggle_mark_at(&mut self, position: usize, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(&ix) = self.visible(View::Repos, store).get(position) else { return };
        let root = store.repos[ix].location.root.clone();
        if !self.marked.remove(&root) {
            self.marked.insert(root);
        }
        cx.notify();
    }

    // ---- actions ------------------------------------------------------------------------------

    pub fn checkout_by_name(&mut self, _: &CheckoutByName, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets(cx);
        if targets.is_empty() {
            return;
        }
        let title = format!("Check out branch in {}", self.describe_targets(&targets, cx));
        self.prompt(title, "", window, cx, move |this, name, window, cx| {
            if !ops::is_valid_branch_name(&name) {
                return this.show_message("Check out", format!("'{name}' is not a valid branch name."), window, cx);
            }
            let check_name = name.clone();
            this.run_batch(
                format!("Check out {name}"),
                "Checking out",
                targets,
                move |s| {
                    if s.head.branch_name() == Some(check_name.as_str()) {
                        Some(Outcome::Done(format!("already on {check_name}")))
                    } else if s.changes.has_tracked_changes() {
                        Some(Outcome::Skipped("uncommitted changes".into()))
                    } else {
                        None
                    }
                },
                move |git, loc, s| {
                    let name = name.clone();
                    async move {
                        Ok(match ops::switch_branch(&git, &loc, &name, &s.remotes).await? {
                            Switched::Local => Outcome::Done(format!("switched to {name}")),
                            Switched::Tracking(start) => Outcome::Done(format!("created {name} from {start}")),
                            Switched::Missing => Outcome::Skipped(format!("no branch {name} (fetch first?)")),
                        })
                    }
                },
                window,
                cx,
            );
        });
    }

    pub fn new_branch_in_repos(&mut self, _: &NewBranchInRepos, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets(cx);
        if targets.is_empty() {
            return;
        }
        let title = format!("New branch in {} (from HEAD)", self.describe_targets(&targets, cx));
        self.prompt(title, "", window, cx, move |this, name, window, cx| {
            if !ops::is_valid_branch_name(&name) {
                return this.show_message("New branch", format!("'{name}' is not a valid branch name."), window, cx);
            }
            this.run_batch(
                format!("New branch {name}"),
                "Creating branch",
                targets,
                |s| matches!(s.head, Head::Unborn(_)).then(|| Outcome::Skipped("no commits yet".into())),
                move |git, loc, _| {
                    let name = name.clone();
                    async move {
                        match ops::new_branch(&git, &loc, &name, None).await {
                            Ok(_) => Ok(Outcome::Done(format!("created {name}"))),
                            Err(err) if err.details().contains("already exists") => {
                                Ok(Outcome::Skipped(format!("{name} already exists")))
                            }
                            Err(err) => Err(err),
                        }
                    }
                },
                window,
                cx,
            );
        });
    }

    pub fn switch_to_default(&mut self, _: &SwitchToDefault, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets(cx);
        let run = move |this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            this.run_batch(
                "Default branch",
                "Checking out",
                targets,
                |s| {
                    if s.default_branch.is_none() {
                        Some(Outcome::Skipped("no default branch found".into()))
                    } else if s.changes.has_tracked_changes() {
                        Some(Outcome::Skipped("uncommitted changes".into()))
                    } else {
                        None
                    }
                },
                |git, loc, s| async move {
                    let Some(default) = &s.default_branch else {
                        return Ok(Outcome::Skipped("no default branch found".into()));
                    };
                    let result = ops::switch_to_default(&git, &loc, default, &s.remotes, s.head.branch_name()).await?;
                    Ok(describe_default(&result))
                },
                window,
                cx,
            );
        };
        match self.marked_targets(cx) {
            Some(roots) if roots.len() > 1 => {
                let message = format!(
                    "Check out the default branch and fast-forward it in {} repos?\n\
                     Repos with uncommitted changes are skipped.",
                    roots.len()
                );
                self.confirm("Default branch", message, window, cx, run);
            }
            _ => run(self, window, cx),
        }
    }

    pub(crate) fn pull_repos(&mut self, roots: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        self.run_batch(
            "Pull",
            "Pulling",
            roots,
            |s| match (&s.head, &s.upstream) {
                (Head::Detached(_), _) => Some(Outcome::Skipped("detached HEAD".into())),
                (_, Upstream::None) => Some(Outcome::Skipped("no upstream".into())),
                (_, Upstream::Gone { .. }) => Some(Outcome::Skipped("upstream gone".into())),
                _ => None,
            },
            |git, loc, _| async move {
                let out = ops::pull(&git, &loc).await?.stdout_str();
                let current = out.contains("Already up to date") || out.contains("is up to date");
                Ok(Outcome::Done(if current { "already up to date" } else { "pulled" }.into()))
            },
            window,
            cx,
        );
    }

    pub(crate) fn push_repos(&mut self, roots: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let names: Vec<String> = {
            let store = self.store.read(cx);
            roots
                .iter()
                .filter_map(|root| store.entry(root))
                .map(|e| {
                    let branch = e.summary.as_ref().and_then(|s| s.head.branch_name()).unwrap_or("-");
                    format!("{}  ({branch})", e.name())
                })
                .collect()
        };
        let message = format!("Push {} repos?\n\n{}", roots.len(), names.join("\n"));
        self.confirm("Push", message, window, cx, move |this, window, cx| {
            this.run_batch(
                "Push",
                "Pushing",
                roots,
                |s| match (&s.head, &s.upstream) {
                    (Head::Detached(_), _) => Some(Outcome::Skipped("detached HEAD".into())),
                    (Head::Unborn(_), _) => Some(Outcome::Skipped("no commits yet".into())),
                    _ if !s.has_remote() => Some(Outcome::Skipped("no remote".into())),
                    (_, Upstream::Gone { .. }) => Some(Outcome::Skipped("upstream gone; push it alone with P".into())),
                    (_, Upstream::Tracking { ahead: 0, .. }) => Some(Outcome::Done("nothing to push".into())),
                    _ => None,
                },
                |git, loc, s| async move {
                    let has_upstream = matches!(s.upstream, Upstream::Tracking { .. });
                    let remote = primary_remote(&s.remotes, None).map(str::to_string);
                    match ops::push(&git, &loc, has_upstream, remote.as_deref(), false).await {
                        Ok(_) if has_upstream => Ok(Outcome::Done("pushed".into())),
                        Ok(_) => Ok(Outcome::Done(format!("pushed to {}", remote.unwrap_or_default()))),
                        // Force pushes stay a deliberate one-repo action (P on that repo).
                        Err(err) if ops::is_push_rejected(&err) => Ok(Outcome::Failed(
                            "rejected: the remote has commits you don't have. Pull first, \
                             or push this repo alone with P to force."
                                .into(),
                        )),
                        Err(err) => Err(err),
                    }
                },
                window,
                cx,
            );
        });
    }

    /// `U`: fast-forward repos that are behind their upstream: the marked ones (saying why
    /// any are skipped), or every repo that can be.
    pub fn fast_forward_all(&mut self, _: &FastForwardAll, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if let Some((done, total)) = store.fetch_round {
            return self.show_message(
                "Fast-forward",
                format!("A fetch is in progress ({done}/{total}). Try again when it finishes."),
                window,
                cx,
            );
        }
        let marked = self.marked_targets(cx);
        let roots = marked.clone().unwrap_or_else(|| {
            store
                .repos
                .iter()
                .filter(|r| precheck(r).is_ok_and(|s| fast_forward_check(s).is_none()))
                .map(|r| r.location.root.clone())
                .collect()
        });
        if roots.is_empty() {
            return self.show_message(
                "Fast-forward",
                "No repos to update: none are on a branch that is behind its upstream and \
                 free of uncommitted changes.\nFetch first with F.",
                window,
                cx,
            );
        }
        let names: Vec<String> = roots
            .iter()
            .filter_map(|root| store.entry(root).map(|e| e.name().to_string()))
            .collect();
        let what = if marked.is_some() { "marked " } else { "" };
        let repos = if roots.len() == 1 { "repo" } else { "repos" };
        let message = format!(
            "Fast-forward {} {what}{repos} to their upstream?\n\n{}",
            roots.len(),
            names.join("\n")
        );
        self.confirm("Fast-forward", message, window, cx, move |this, window, cx| {
            this.run_batch(
                "Fast-forward",
                "Updating",
                roots,
                fast_forward_check,
                |git, loc, s| async move {
                    let behind = match s.upstream {
                        Upstream::Tracking { behind, .. } => behind,
                        _ => 0,
                    };
                    ops::fast_forward_head(&git, &loc).await?;
                    Ok(Outcome::Done(format!("fast-forwarded {}", commits(behind))))
                },
                window,
                cx,
            );
        });
    }

    // ---- running --------------------------------------------------------------------------

    /// Runs `op` on every repo in `targets` that passes the common checks and `check`
    /// (which returns an outcome to settle a repo without running anything). With several
    /// repos a popup shows each outcome as it lands; with one, only a skip or failure is
    /// shown.
    #[allow(clippy::too_many_arguments)] // two of them are gpui's usual window and cx
    fn run_batch<Fut>(
        &mut self,
        title: impl Into<SharedString>,
        label: &'static str,
        targets: Vec<PathBuf>,
        check: impl Fn(&RepoSummary) -> Option<Outcome>,
        op: impl Fn(Git, RepoLocation, RepoSummary) -> Fut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Fut: Future<Output = Result<Outcome, GitError>> + Send + 'static,
    {
        let title: SharedString = title.into();
        let mut rows = Vec::new();
        let mut jobs = Vec::new();
        let store = self.store.read(cx);
        for root in targets {
            let Some(entry) = store.entry(&root) else { continue };
            let outcome = match precheck(entry) {
                Err(outcome) => outcome,
                Ok(summary) => check(summary).unwrap_or_else(|| {
                    jobs.push((rows.len(), entry.name().to_string(), root.clone(), summary.clone()));
                    Outcome::Pending
                }),
            };
            rows.push(BatchRow { name: entry.name().to_string(), outcome });
        }
        self.last_batch += 1;
        let id = self.last_batch;
        let single = rows.len() == 1;
        if single {
            if let Some(message) = problem(&rows[0].outcome) {
                self.show_message(&title, format!("{}: {message}", rows[0].name), window, cx);
            }
        } else {
            let dialog = Dialog::Results { id, title: title.clone(), rows, scroll: UniformListScrollHandle::new() };
            self.open_dialog(dialog, window, cx);
        }
        for (ix, name, root, summary) in jobs {
            let task = self
                .store
                .update(cx, |store, cx| store.run_op(&root, label, |git, loc| op(git, loc, summary), cx));
            let title = title.clone();
            cx.spawn_in(window, async move |this, cx| {
                let outcome = task.await.unwrap_or_else(|err| Outcome::Failed(err.details()));
                this.update_in(cx, |this, window, cx| {
                    if single {
                        if let Some(message) = problem(&outcome) {
                            this.show_message(&title, format!("{name}: {message}"), window, cx);
                        }
                    } else if let Some(Dialog::Results { id: showing, rows, .. }) = &mut this.dialog
                        && *showing == id
                    {
                        rows[ix].outcome = outcome;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }
}

/// Why nothing can run on a repo right now, whatever the action.
fn precheck(entry: &RepoEntry) -> Result<&RepoSummary, Outcome> {
    let skip = |reason: String| Err(Outcome::Skipped(reason));
    if let Some(busy) = &entry.busy {
        return skip(format!("busy ({busy}), try again"));
    }
    if entry.error.is_some() {
        return skip("can't be read, see its Status".into());
    }
    let Some(summary) = &entry.summary else {
        return skip("still loading".into());
    };
    if entry.location.bare {
        return skip("bare repository".into());
    }
    if let Some(op) = &summary.op {
        return skip(format!("{} in progress", op.label()));
    }
    Ok(summary)
}

fn fast_forward_check(s: &RepoSummary) -> Option<Outcome> {
    let skip = |reason: &str| Some(Outcome::Skipped(reason.into()));
    match &s.upstream {
        _ if matches!(s.head, Head::Detached(_)) => skip("detached HEAD"),
        _ if s.changes.has_tracked_changes() => skip("uncommitted changes"),
        Upstream::None => skip("no upstream"),
        Upstream::Gone { .. } => skip("upstream gone"),
        Upstream::Tracking { behind: 0, .. } => Some(Outcome::Done("already up to date".into())),
        Upstream::Tracking { ahead, .. } if *ahead > 0 => skip("diverged from upstream, can't fast-forward"),
        Upstream::Tracking { .. } => None,
    }
}

fn describe_default(r: &DefaultSwitch) -> Outcome {
    let branch = &r.branch;
    let lead = match &r.switched {
        Some(Switched::Missing) => return Outcome::Skipped(format!("no branch {branch}")),
        Some(Switched::Local) => format!("switched to {branch}"),
        Some(Switched::Tracking(start)) => format!("created {branch} from {start}"),
        None => format!("on {branch}"),
    };
    match &r.upstream {
        None => Outcome::Done(format!("{lead}, no upstream")),
        Some(_) if r.fast_forwarded => Outcome::Done(format!("{lead}, fast-forwarded {}", commits(r.behind))),
        Some(upstream) if r.behind > 0 => Outcome::Failed(format!(
            "{lead}, but it has diverged from {upstream} (↑{} ↓{}), so it wasn't fast-forwarded",
            r.ahead, r.behind
        )),
        Some(upstream) if r.ahead > 0 => Outcome::Done(format!("{lead}, {} ahead of {upstream}", commits(r.ahead))),
        Some(_) => Outcome::Done(format!("{lead}, up to date")),
    }
}

fn commits(n: u32) -> String {
    if n == 1 { "1 commit".into() } else { format!("{n} commits") }
}

/// The message to show for a skipped or failed repo.
fn problem(outcome: &Outcome) -> Option<&str> {
    match outcome {
        Outcome::Skipped(message) | Outcome::Failed(message) => Some(message),
        Outcome::Pending | Outcome::Done(_) => None,
    }
}

/// The line of git's error output that says what went wrong, without `fatal:`/`error:`.
pub fn headline(details: &str) -> &str {
    let lines = || details.lines().map(str::trim).filter(|l| !l.is_empty());
    lines()
        .find_map(|l| l.strip_prefix("fatal: ").or_else(|| l.strip_prefix("error: ")))
        .or_else(|| lines().find(|l| !l.starts_with("hint:")))
        .unwrap_or(details)
}
