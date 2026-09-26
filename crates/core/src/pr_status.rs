//! Keeps each repo's pull request current: which branch to ask GitHub about, when to ask
//! again, and what to show meanwhile. The app runs [`look_up`] in the background, one at a
//! time, as [`PrQueue`] says; the rules live here so they can be tested.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::gh::{Gh, GhError};
use crate::git::Git;
use crate::github::{self, PrLookup, PrTarget, RemoteRepo};
use crate::model::{ChecksState, Head, PrState, PullRequest, RepoSummary, Upstream};
use crate::ops::default_branch_parts;

/// How long a repo whose branch changed waits for more changes before its PR is looked
/// up, so a checkout across many repos makes one lookup, and GitHub has seen a push.
pub const DEBOUNCE: Duration = Duration::from_secs(5);
/// The longest a stream of changes can put a lookup off.
pub const MAX_DEBOUNCE: Duration = Duration::from_secs(20);
/// How often PRs whose checks are still running are looked up again. Only those: a full
/// round of ~120 repos costs 51 of GitHub's 5,000 points an hour.
pub const PENDING_INTERVAL: Duration = Duration::from_secs(60);
/// How long checks can run before [`PENDING_INTERVAL`] gives up on them and they wait for
/// full rounds. A check that never reports, like a required one no workflow runs, would
/// otherwise cost a lookup a minute for as long as the app is open.
pub const PENDING_LIMIT: Duration = Duration::from_secs(30 * 60);
/// `R` within this long of the last round, say pressed twice, doesn't make another.
pub const REFRESH_AGAIN: Duration = Duration::from_secs(5);

/// What a repo's PR is looked up for: the branch it would come from, on its remote, and
/// that branch's commit there. A push, a fetch or pull that moves it, a checkout or a new
/// upstream all change it, and then the PR is looked up again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub remote: String,
    /// The branch's name on the remote, which is what a PR's head names.
    pub branch: String,
    /// [`RepoSummary::remote_branch_oid`]; `None` before the branch is pushed.
    pub remote_oid: Option<String>,
}

impl PrKey {
    /// Whether an answer for `other` is about this branch, if maybe an older commit of it.
    pub fn same_branch(&self, other: &PrKey) -> bool {
        self.remote == other.remote && self.branch == other.branch
    }
}

/// Why a repo's PR isn't looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    Bare,
    Detached,
    /// A branch without commits.
    Unborn,
    /// On the default branch, which PRs go into rather than come from. Also a branch that
    /// tracks it under another name, until it's pushed with its own upstream.
    DefaultBranch,
    NoRemote,
    /// The remote's URL doesn't name a GitHub repo, or not on a host gh is logged in to.
    NotOnGitHub,
}

/// The branch to look up a repo's PR for, or why there's none.
pub fn pr_key(summary: &RepoSummary, bare: bool) -> Result<PrKey, Skip> {
    if bare {
        return Err(Skip::Bare);
    }
    let local = match &summary.head {
        Head::Branch(name) => name,
        Head::Detached(_) => return Err(Skip::Detached),
        Head::Unborn(_) => return Err(Skip::Unborn),
    };
    let upstream = match &summary.upstream {
        Upstream::Tracking { name, .. } | Upstream::Gone { name } => Some(name.as_str()),
        Upstream::None => None,
    };
    let (remote, branch) = github::remote_branch(upstream, &summary.remotes, local).ok_or(Skip::NoRemote)?;
    if let Some(default) = &summary.default_branch
        && let (name, Some(default_remote)) = default_branch_parts(default, &summary.remotes)
        && default_remote == remote
        && name == branch
    {
        return Err(Skip::DefaultBranch);
    }
    Ok(PrKey {
        remote,
        branch,
        remote_oid: summary.remote_branch_oid.clone(),
    })
}

/// Whether gh can be asked, for every repo at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhStatus {
    /// `github_status = false`: gh never runs.
    Disabled,
    /// Not asked yet.
    Unchecked,
    /// Checked again only on `R`.
    NotInstalled,
    /// No working login on any host. Checked again every full round, so a `gh auth login`
    /// in a terminal is picked up.
    NotLoggedIn,
    /// Asking which hosts gh is logged in to failed, e.g. it timed out, with no hosts known
    /// from before. Asked again with the next round or `R`; lookups wait till then.
    Failed(String),
    /// The hosts gh has a working login for, the only ones asked about.
    Ready { hosts: Vec<String> },
}

impl GhStatus {
    pub fn hosts(&self) -> Option<&[String]> {
        match self {
            GhStatus::Ready { hosts } => Some(hosts),
            _ => None,
        }
    }
}

/// The latest answer about a repo's PR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// What it was looked up for.
    pub key: PrKey,
    pub lookup: PrLookup,
    /// When the answer came.
    pub checked: SystemTime,
    /// Since when the PR's checks have been running, in the answers for this key: a push
    /// starts them again.
    pub pending_since: Option<SystemTime>,
}

/// Why the latest lookup of a repo's PR failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupError {
    pub key: PrKey,
    pub message: String,
    pub at: SystemTime,
}

/// What's known about one repo's pull request. See [`view`] for what of it shows.
#[derive(Clone, Debug, Default)]
pub struct RepoPr {
    /// The latest answer. It's kept across checkouts but only shows while it's for the
    /// checked-out branch.
    pub found: Option<Found>,
    /// Set when the latest lookup failed; `found` stays as it was.
    pub error: Option<LookupError>,
    /// Each remote's GitHub repo as `git remote get-url --push` names it, `None` when the
    /// URL names none. Read when first needed, and again by every full round and rescan,
    /// in case `git remote set-url` changed it.
    pub remotes: HashMap<String, Option<RemoteRepo>>,
    /// What the repo's PR was last queued for, and the local tip then.
    queued: Option<(PrKey, Option<String>)>,
}

impl RepoPr {
    /// Takes in what a fresh summary says the PR is for; true when it should be looked up
    /// again. That's when it was never looked up, for another branch or remote commit, or
    /// when the local tip moved under a merged PR, which only counts while it's the tip.
    /// Only a change counts, so a lookup that fails isn't retried on every refresh.
    pub fn update_key(&mut self, key: &PrKey, local_oid: Option<&str>) -> bool {
        let merged = self.found.as_ref().is_some_and(|found| {
            found.key.same_branch(key) && found.lookup.pr.as_ref().is_some_and(|pr| pr.state == PrState::Merged)
        });
        let changed = match &self.queued {
            None => true,
            Some((queued, tip)) => queued != key || (merged && tip.as_deref() != local_oid),
        };
        if changed {
            self.queued = Some((key.clone(), local_oid.map(Into::into)));
        }
        changed
    }

    /// The repo was queued but left out of the lookup, say because it was checked out
    /// detached meanwhile: its key counts as a change again, or going back to the branch
    /// would never look it up.
    pub fn unqueue(&mut self) {
        self.queued = None;
    }

    /// Takes in a lookup's answer for this repo.
    pub fn record(&mut self, answered: Answered, now: SystemTime) {
        if let Some(remote) = answered.remote {
            self.remotes.insert(answered.key.remote.clone(), remote);
        }
        match answered.answer {
            Answer::Lookup(lookup) => {
                let pending_since = lookup.pr.as_ref().filter(|pr| checks_running(pr)).map(|_| {
                    self.found
                        .as_ref()
                        .filter(|found| found.key == answered.key)
                        .and_then(|found| found.pending_since)
                        .unwrap_or(now)
                });
                self.found = Some(Found {
                    key: answered.key,
                    lookup,
                    checked: now,
                    pending_since,
                });
                self.error = None;
            }
            Answer::Failed(message) => {
                self.error = Some(LookupError {
                    key: answered.key,
                    message,
                    at: now,
                })
            }
            // Nothing wrong with the repo: `view` says why there's no PR.
            Answer::NotOnGitHub | Answer::NotAsked => self.error = None,
        }
    }
}

/// What to show for a repo's PR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrView<'a> {
    /// `github_status = false`.
    Off,
    Skipped(Skip),
    GhNotInstalled,
    GhNotLoggedIn,
    /// Asking gh which hosts it's logged in to failed.
    GhFailed(&'a str),
    /// Nothing known: the repo hasn't been read (or can't be), gh hasn't been asked yet, or
    /// the branch's lookup hasn't answered.
    Unknown,
    /// The latest answer for the checked-out branch: its PR, or none and where to open
    /// one. `error` is set when a later lookup failed.
    Found {
        found: &'a Found,
        error: Option<&'a LookupError>,
    },
    /// The lookup for the checked-out branch failed, and there's no earlier answer.
    Failed(&'a LookupError),
}

/// What to show for a repo's PR, given gh's status and the repo's current summary. An
/// answer only shows while it's for the checked-out branch, so a checkout never shows the
/// previous branch's PR while the new one is looked up. `summary` is `None` for a repo not
/// read yet, or that can't be read.
pub fn view<'a>(gh: &'a GhStatus, summary: Option<&RepoSummary>, bare: bool, pr: &'a RepoPr) -> PrView<'a> {
    if *gh == GhStatus::Disabled {
        return PrView::Off;
    }
    let Some(summary) = summary else {
        return PrView::Unknown;
    };
    let key = match pr_key(summary, bare) {
        Ok(key) => key,
        Err(skip) => return PrView::Skipped(skip),
    };
    let remote = pr.remotes.get(&key.remote);
    if let Some(None) = remote {
        return PrView::Skipped(Skip::NotOnGitHub);
    }
    match gh {
        GhStatus::Disabled => return PrView::Off,
        GhStatus::Unchecked => return PrView::Unknown,
        GhStatus::NotInstalled => return PrView::GhNotInstalled,
        GhStatus::NotLoggedIn => return PrView::GhNotLoggedIn,
        GhStatus::Failed(message) => return PrView::GhFailed(message),
        GhStatus::Ready { hosts } => {
            if let Some(Some(repo)) = remote
                && !hosts.contains(&repo.host)
            {
                return PrView::Skipped(Skip::NotOnGitHub);
            }
        }
    }
    let error = pr.error.as_ref().filter(|error| error.key.same_branch(&key));
    let found = pr.found.as_ref().filter(|found| {
        found.key.same_branch(&key)
            // New local commits: that merged PR isn't this branch's any more.
            && !found.lookup.pr.as_ref().is_some_and(|pr| {
                pr.state == PrState::Merged && summary.head_oid.as_ref() != Some(&pr.head_oid)
            })
    });
    match (found, error) {
        (Some(found), error) => PrView::Found { found, error },
        (None, Some(error)) => PrView::Failed(error),
        (None, None) => PrView::Unknown,
    }
}

/// Whether a PR's checks are still running. A merged PR's don't matter any more.
pub fn checks_running(pr: &PullRequest) -> bool {
    pr.state != PrState::Merged
        && pr
            .checks
            .as_ref()
            .is_some_and(|checks| checks.state == ChecksState::Pending || checks.pending > 0)
}

/// Whether to look a shown PR up again because its checks were still running. Not when it
/// was looked up less than half of [`PENDING_INTERVAL`] ago, say by a full round, nor once
/// they've been running for [`PENDING_LIMIT`].
pub fn recheck_due(view: &PrView, now: SystemTime) -> bool {
    let since = |time: SystemTime| now.duration_since(time).unwrap_or_default();
    match view {
        PrView::Found { found, .. } => {
            found.lookup.pr.as_ref().is_some_and(checks_running)
                && since(found.checked) >= PENDING_INTERVAL / 2
                && found.pending_since.is_none_or(|pending| since(pending) < PENDING_LIMIT)
        }
        _ => false,
    }
}

/// When the queued repos may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    Now,
    /// After [`DEBOUNCE`] without more changes, but no later than [`MAX_DEBOUNCE`] after
    /// the first.
    After { first: Instant, at: Instant },
}

/// Repos waiting for their PR to be looked up. One lookup runs at a time; whatever is
/// queued meanwhile goes into the next.
#[derive(Debug, Default)]
pub struct PrQueue {
    repos: BTreeSet<PathBuf>,
    /// Ask gh which hosts it's logged in to before looking anything up.
    check_hosts: bool,
    due: Option<Due>,
    running: bool,
    /// The running lookup asks gh which hosts it's logged in to.
    running_check: bool,
}

/// What [`PrQueue::next`] says to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing, until more is queued or the running lookup finishes.
    Idle,
    /// Call again then.
    Wait(Instant),
    /// Start a lookup ([`look_up`]) and call [`PrQueue::finished`] when it's done.
    Start { repos: Vec<PathBuf>, check_hosts: bool },
}

impl PrQueue {
    /// Queues a repo whose PR key changed ([`RepoPr::update_key`]).
    pub fn changed(&mut self, repo: PathBuf, now: Instant) {
        self.repos.insert(repo);
        self.due = Some(match self.due {
            Some(Due::Now) => Due::Now,
            Some(Due::After { first, .. }) => Due::After {
                first,
                at: (now + DEBOUNCE).min(first + MAX_DEBOUNCE),
            },
            None => Due::After {
                first: now,
                at: now + DEBOUNCE,
            },
        });
    }

    /// Queues repos to look up at once, with everything already waiting.
    pub fn now(&mut self, repos: impl IntoIterator<Item = PathBuf>) {
        self.repos.extend(repos);
        self.due = Some(Due::Now);
    }

    /// A timer round: every repo, after asking gh which hosts it's logged in to, which
    /// picks up a `gh auth login` in a terminal. While gh isn't installed, nothing: that
    /// waits for `R` rather than trying every round.
    pub fn round(&mut self, repos: impl IntoIterator<Item = PathBuf>, gh: &GhStatus) {
        if matches!(gh, GhStatus::Disabled | GhStatus::NotInstalled) {
            return;
        }
        self.check_hosts = true;
        self.now(repos);
    }

    /// `R`: every repo, after checking gh again whatever it said before. Not when the last
    /// round started less than [`REFRESH_AGAIN`] ago (`since_round`), or while gh's hosts
    /// are being checked or about to be: pressing `R` again then only costs another round.
    /// Returns whether it queued the round.
    pub fn refresh(&mut self, repos: impl IntoIterator<Item = PathBuf>, since_round: Duration) -> bool {
        if since_round < REFRESH_AGAIN || self.check_hosts || self.running_check {
            return false;
        }
        self.check_hosts = true;
        self.now(repos);
        true
    }

    /// What to do now.
    pub fn next(&mut self, gh: &GhStatus, now: Instant) -> Next {
        if self.running {
            return Next::Idle;
        }
        let blocked = match gh {
            GhStatus::Disabled => true,
            // Until a round or `R` checks gh again. Asking which hosts to use on every change
            // would run gh for nothing.
            GhStatus::NotInstalled | GhStatus::NotLoggedIn | GhStatus::Failed(_) => !self.check_hosts,
            GhStatus::Unchecked | GhStatus::Ready { .. } => false,
        };
        if blocked || (self.repos.is_empty() && !self.check_hosts) {
            *self = Self::default();
            return Next::Idle;
        }
        match self.due {
            Some(Due::After { at, .. }) if at > now => return Next::Wait(at),
            Some(_) => {}
            None => return Next::Idle,
        }
        let check_hosts = self.check_hosts || *gh == GhStatus::Unchecked;
        let repos = std::mem::take(&mut self.repos).into_iter().collect();
        *self = Self {
            running: true,
            running_check: check_hosts,
            ..Self::default()
        };
        Next::Start { repos, check_hosts }
    }

    pub fn finished(&mut self) {
        self.running = false;
        self.running_check = false;
    }
}

/// A repo to look up.
#[derive(Clone, Debug)]
pub struct Request {
    pub root: PathBuf,
    pub key: PrKey,
    /// The local branch tip: a merged PR only counts while it's this.
    pub local_oid: Option<String>,
    /// The remote's GitHub repo when its URL was read before; `None` reads it.
    pub remote: Option<Option<RemoteRepo>>,
}

/// The answer about one repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Lookup(PrLookup),
    NotOnGitHub,
    Failed(String),
    /// gh couldn't be asked; the lookup's [`GhStatus`] says why.
    NotAsked,
}

#[derive(Clone, Debug)]
pub struct Answered {
    pub root: PathBuf,
    pub key: PrKey,
    /// The remote's GitHub repo, to keep, when the URL could be read.
    pub remote: Option<Option<RemoteRepo>>,
    pub answer: Answer,
}

#[derive(Clone, Debug)]
pub struct Lookups {
    /// gh's new status, when it changed or was checked.
    pub gh: Option<GhStatus>,
    /// One per request, in order.
    pub repos: Vec<Answered>,
}

/// Looks up the PRs for `requests`. Reads the remote URLs not known yet, then asks gh which
/// hosts it's logged in to when `check_hosts`, else takes `hosts` from the last time, and
/// asks each host about its repos with one [`github::lookup`]. gh is never run for a host
/// it isn't logged in to (see [`github::logged_in_hosts`]). gh runs in `cwd`.
pub async fn look_up(
    git: &Git,
    gh: &Gh,
    cwd: &Path,
    check_hosts: bool,
    hosts: Option<Vec<String>>,
    requests: Vec<Request>,
) -> Lookups {
    // First, as they need no gh: a URL that names no GitHub repo is "not on GitHub" even
    // while gh is missing or logged out. The push URL, since the branch, and so a PR's
    // head, is where it's pushed: a remote can fetch from the upstream and push to a fork.
    // `get-url` also applies `pushInsteadOf` and `insteadOf`, which the configured URL
    // doesn't show.
    let remotes: Vec<Result<Option<RemoteRepo>, String>> =
        futures::future::join_all(requests.iter().map(|request| async move {
            match &request.remote {
                Some(known) => Ok(known.clone()),
                None => git
                    .read(&request.root, ["remote", "get-url", "--push", request.key.remote.as_str()])
                    .await
                    .map(|out| github::parse_remote_url(&out.stdout_str()))
                    .map_err(|err| err.to_string()),
            }
        }))
        .await;

    let mut status = None;
    let mut hosts = hosts;
    if check_hosts {
        (status, hosts) = match github::logged_in_hosts(gh, cwd).await {
            Ok(found) if found.is_empty() => (Some(GhStatus::NotLoggedIn), None),
            Ok(found) => (Some(GhStatus::Ready { hosts: found.clone() }), Some(found)),
            Err(GhError::NotInstalled) => (Some(GhStatus::NotInstalled), None),
            Err(GhError::NotLoggedIn { .. }) => (Some(GhStatus::NotLoggedIn), None),
            // Nothing learned: keep going with the hosts from last time, if any.
            Err(err) if hosts.is_none() => (Some(GhStatus::Failed(err.to_string())), None),
            Err(_) => (None, hosts),
        };
    }

    let mut answers: Vec<Option<Answer>> = vec![None; requests.len()];
    let mut by_host: BTreeMap<&str, Vec<(usize, &RemoteRepo)>> = BTreeMap::new();
    for (i, remote) in remotes.iter().enumerate() {
        match (remote, &hosts) {
            (Ok(Some(repo)), Some(hosts)) if hosts.contains(&repo.host) => {
                by_host.entry(repo.host.as_str()).or_default().push((i, repo))
            }
            // Without hosts, only a URL that names no GitHub repo says anything.
            (Ok(Some(_)), None) => {}
            (Ok(_), _) => answers[i] = Some(Answer::NotOnGitHub),
            (Err(message), _) => answers[i] = Some(Answer::Failed(message.clone())),
        }
    }
    let mut logged_out: Vec<&str> = Vec::new();
    for (host, repos) in by_host {
        let targets: Vec<PrTarget> = repos
            .iter()
            .map(|&(i, repo)| PrTarget {
                repo: repo.clone(),
                branch: requests[i].key.branch.clone(),
                local_oid: requests[i].local_oid.clone(),
            })
            .collect();
        let failed = match github::lookup(gh, host, &targets, cwd).await {
            Ok(results) => {
                for (&(i, _), result) in repos.iter().zip(results) {
                    answers[i] = Some(result.map_or_else(Answer::Failed, Answer::Lookup));
                }
                continue;
            }
            // gh itself is missing: every other host would fail the same way.
            Err(GhError::NotInstalled) => {
                status = Some(GhStatus::NotInstalled);
                break;
            }
            // Logins are per host, so the other hosts may still work. This one is left out
            // until gh's hosts are checked again.
            Err(GhError::NotLoggedIn { .. }) => {
                logged_out.push(host);
                format!("gh isn't logged in to {host}")
            }
            Err(err) => err.to_string(),
        };
        for &(i, _) in &repos {
            answers[i] = Some(Answer::Failed(failed.clone()));
        }
    }
    if !logged_out.is_empty()
        && status != Some(GhStatus::NotInstalled)
        && let Some(hosts) = &hosts
    {
        let left: Vec<String> = hosts.iter().filter(|host| !logged_out.contains(&host.as_str())).cloned().collect();
        status = Some(if left.is_empty() { GhStatus::NotLoggedIn } else { GhStatus::Ready { hosts: left } });
    }

    let repos = requests
        .into_iter()
        .zip(remotes)
        .zip(answers)
        .map(|((request, remote), answer)| answered(request, remote.ok(), answer.unwrap_or(Answer::NotAsked)))
        .collect();
    Lookups { gh: status, repos }
}

fn answered(request: Request, remote: Option<Option<RemoteRepo>>, answer: Answer) -> Answered {
    Answered {
        root: request.root,
        key: request.key,
        remote,
        answer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChangeCounts, Checks, DefaultBranch};
    use async_io::block_on;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    fn summary(head: Head, upstream: Upstream, remotes: &[&str]) -> RepoSummary {
        RepoSummary {
            head_oid: match &head {
                Head::Unborn(_) => None,
                _ => Some("local".into()),
            },
            head,
            upstream,
            upstream_oid: None,
            remote_branch_oid: Some("remote".into()),
            base: None,
            changes: ChangeCounts::default(),
            stash_count: 0,
            op: None,
            last_fetch: None,
            remotes: remotes.iter().map(|r| r.to_string()).collect(),
            default_branch: remotes.first().map(|remote| DefaultBranch {
                full_ref: format!("refs/remotes/{remote}/main"),
                short: format!("{remote}/main"),
            }),
            shallow: false,
        }
    }

    fn on_branch(branch: &str) -> RepoSummary {
        let upstream = Upstream::Tracking {
            name: format!("origin/{branch}"),
            ahead: 0,
            behind: 0,
        };
        summary(Head::Branch(branch.into()), upstream, &["origin"])
    }

    fn key(branch: &str, remote_oid: &str) -> PrKey {
        PrKey {
            remote: "origin".into(),
            branch: branch.into(),
            remote_oid: Some(remote_oid.into()),
        }
    }

    #[test]
    fn keys_follow_the_remote_branch_and_skip_what_has_no_pr() {
        assert_eq!(pr_key(&on_branch("feat"), false), Ok(key("feat", "remote")));
        // Pushed without -u: the same name on the primary remote.
        let pushed = summary(Head::Branch("wip".into()), Upstream::None, &["origin"]);
        assert_eq!(pr_key(&pushed, false), Ok(key("wip", "remote")));
        let gone = Upstream::Gone {
            name: "fork/old".into(),
        };
        let fork = summary(Head::Branch("local".into()), gone, &["origin", "fork"]);
        assert_eq!(pr_key(&fork, false).map(|k| (k.remote, k.branch)), Ok(("fork".into(), "old".into())));

        assert_eq!(pr_key(&on_branch("feat"), true), Err(Skip::Bare));
        let detached = summary(Head::Detached("abc".into()), Upstream::None, &["origin"]);
        assert_eq!(pr_key(&detached, false), Err(Skip::Detached));
        let unborn = summary(Head::Unborn("main".into()), Upstream::None, &["origin"]);
        assert_eq!(pr_key(&unborn, false), Err(Skip::Unborn));
        let local = summary(Head::Branch("feat".into()), Upstream::None, &[]);
        assert_eq!(pr_key(&local, false), Err(Skip::NoRemote));
        assert_eq!(pr_key(&on_branch("main"), false), Err(Skip::DefaultBranch));
        // A branch started from origin/main keeps it as its upstream until pushed.
        let started = Upstream::Tracking {
            name: "origin/main".into(),
            ahead: 1,
            behind: 0,
        };
        let started = summary(Head::Branch("feat".into()), started, &["origin"]);
        assert_eq!(pr_key(&started, false), Err(Skip::DefaultBranch));
        // `main` on a fork's remote isn't the default branch's remote's.
        let fork_main = Upstream::Tracking {
            name: "fork/main".into(),
            ahead: 0,
            behind: 0,
        };
        let mut fork_main = summary(Head::Branch("main".into()), fork_main, &["origin", "fork"]);
        fork_main.default_branch = Some(DefaultBranch {
            full_ref: "refs/remotes/origin/main".into(),
            short: "origin/main".into(),
        });
        assert_eq!(pr_key(&fork_main, false).map(|k| k.remote), Ok("fork".into()));
    }

    fn pr(state: PrState, head_oid: &str, checks: Option<ChecksState>) -> PullRequest {
        PullRequest {
            number: 18,
            title: "t".into(),
            url: "https://github.com/o/r/pull/18".into(),
            state,
            review: None,
            checks: checks.map(|state| Checks {
                state,
                failing: vec![],
                total: 1,
                pending: u32::from(state == ChecksState::Pending),
            }),
            reviewers: vec![],
            base_ref: "main".into(),
            base_repo: "o/r".into(),
            head_oid: head_oid.into(),
        }
    }

    fn github_repo(host: &str) -> RemoteRepo {
        RemoteRepo {
            host: host.into(),
            owner: "o".into(),
            name: "r".into(),
        }
    }

    fn answer(key: PrKey, answer: Answer) -> Answered {
        Answered {
            root: "/w/r".into(),
            key,
            remote: Some(Some(github_repo("github.com"))),
            answer,
        }
    }

    fn with_pr(key: PrKey, pr: Option<PullRequest>, at: SystemTime) -> RepoPr {
        let mut state = RepoPr::default();
        let lookup = PrLookup { pr, create_url: None };
        state.record(answer(key, Answer::Lookup(lookup)), at);
        state
    }

    #[test]
    fn a_lookup_is_needed_when_the_key_changes() {
        let mut state = RepoPr::default();
        assert!(state.update_key(&key("feat", "a"), Some("local")));
        // Refreshes that change nothing, even after a failed lookup.
        state.record(answer(key("feat", "a"), Answer::Failed("timeout".into())), SystemTime::now());
        assert!(!state.update_key(&key("feat", "a"), Some("local")));
        assert!(!state.update_key(&key("feat", "a"), Some("new local commit")));
        // A push, then a checkout.
        assert!(state.update_key(&key("feat", "b"), Some("local")));
        assert!(state.update_key(&key("other", "b"), Some("local")));
        assert!(!state.update_key(&key("other", "b"), Some("local")));

        // Under a merged PR, a new local tip matters too.
        let mut state = with_pr(key("feat", "a"), Some(pr(PrState::Merged, "local", None)), SystemTime::now());
        assert!(state.update_key(&key("feat", "a"), Some("local")));
        assert!(!state.update_key(&key("feat", "a"), Some("local")));
        assert!(state.update_key(&key("feat", "a"), Some("new")));
        assert!(!state.update_key(&key("feat", "a"), Some("new")));
        let mut state = with_pr(key("feat", "a"), Some(pr(PrState::Open, "local", None)), SystemTime::now());
        assert!(state.update_key(&key("feat", "a"), Some("local")));
        assert!(!state.update_key(&key("feat", "a"), Some("new")));

        // Queued, then left out of the lookup (detached meanwhile, say): back on the
        // branch, the same key is looked up after all.
        let mut state = RepoPr::default();
        assert!(state.update_key(&key("feat", "a"), Some("local")));
        state.unqueue();
        assert!(state.update_key(&key("feat", "a"), Some("local")));
        assert!(!state.update_key(&key("feat", "a"), Some("local")));
    }

    fn ready() -> GhStatus {
        GhStatus::Ready {
            hosts: vec!["github.com".into()],
        }
    }

    /// The PR number [`view`] shows, or what it shows instead.
    fn shown(gh: &GhStatus, summary: &RepoSummary, state: &RepoPr) -> Result<Option<u64>, String> {
        match view(gh, Some(summary), false, state) {
            PrView::Found { found, error: None } => Ok(found.lookup.pr.as_ref().map(|pr| pr.number)),
            other => Err(format!("{other:?}")),
        }
    }

    #[test]
    fn shows_an_answer_only_for_the_checked_out_branch() {
        let now = SystemTime::now();
        let state = with_pr(key("feat", "remote"), Some(pr(PrState::Open, "remote", None)), now);
        assert_eq!(shown(&ready(), &on_branch("feat"), &state), Ok(Some(18)));
        // After a checkout, before the other branch's answer.
        assert_eq!(view(&ready(), Some(&on_branch("other")), false, &state), PrView::Unknown);
        // After a push, until the answer for the new commit: still that branch's PR.
        let mut pushed = on_branch("feat");
        pushed.remote_branch_oid = Some("pushed".into());
        assert_eq!(shown(&ready(), &pushed, &state), Ok(Some(18)));
        // A merged PR, once the local tip moved on from it.
        let merged = with_pr(key("feat", "remote"), Some(pr(PrState::Merged, "local", None)), now);
        assert_eq!(shown(&ready(), &on_branch("feat"), &merged), Ok(Some(18)));
        let mut moved = on_branch("feat");
        moved.head_oid = Some("new".into());
        assert_eq!(view(&ready(), Some(&moved), false, &merged), PrView::Unknown);
    }

    #[test]
    fn a_failed_lookup_keeps_the_last_answer() {
        let now = SystemTime::now();
        let mut state = with_pr(key("feat", "a"), None, now);
        state.record(answer(key("feat", "b"), Answer::Failed("timed out".into())), now);
        let summary = on_branch("feat");
        match view(&ready(), Some(&summary), false, &state) {
            PrView::Found { found, error: Some(error) } => {
                assert_eq!(found.key, key("feat", "a"));
                assert_eq!(error.message, "timed out");
            }
            other => panic!("{other:?}"),
        }
        // Nothing earlier for this branch.
        state.record(answer(key("other", "b"), Answer::Failed("boom".into())), now);
        let other = on_branch("other");
        assert!(matches!(view(&ready(), Some(&other), false, &state), PrView::Failed(e) if e.message == "boom"));
        // An answer clears it.
        state.record(answer(key("other", "b"), Answer::Lookup(PrLookup { pr: None, create_url: None })), now);
        assert_eq!(shown(&ready(), &other, &state), Ok(None));
    }

    #[test]
    fn view_says_why_there_is_no_pr() {
        let state = with_pr(key("feat", "remote"), None, SystemTime::now());
        let feat = on_branch("feat");
        let at = |gh: &GhStatus, summary: Option<&RepoSummary>, state: &RepoPr| format!("{:?}", view(gh, summary, false, state));
        assert_eq!(at(&GhStatus::Disabled, Some(&feat), &state), "Off");
        assert_eq!(at(&ready(), None, &state), "Unknown");
        assert_eq!(at(&ready(), Some(&on_branch("main")), &state), "Skipped(DefaultBranch)");
        assert_eq!(at(&GhStatus::Unchecked, Some(&feat), &state), "Unknown");
        assert_eq!(at(&GhStatus::NotInstalled, Some(&feat), &state), "GhNotInstalled");
        assert_eq!(at(&GhStatus::NotLoggedIn, Some(&feat), &state), "GhNotLoggedIn");
        assert_eq!(at(&GhStatus::Failed("x".into()), Some(&feat), &state), "GhFailed(\"x\")");

        let mut gitlab = state.clone();
        gitlab.remotes.insert("origin".into(), None);
        assert_eq!(at(&ready(), Some(&feat), &gitlab), "Skipped(NotOnGitHub)");
        // Whatever gh says: the URL decides.
        assert_eq!(at(&GhStatus::NotLoggedIn, Some(&feat), &gitlab), "Skipped(NotOnGitHub)");
        let mut ghe = state.clone();
        ghe.remotes.insert("origin".into(), Some(github_repo("ghe.example.com")));
        assert_eq!(at(&ready(), Some(&feat), &ghe), "Skipped(NotOnGitHub)");
    }

    #[test]
    fn rechecks_prs_whose_checks_are_running() {
        let then = SystemTime::now();
        let feat = on_branch("feat");
        let due = |pr: PullRequest, after: Duration| {
            let state = with_pr(key("feat", "remote"), Some(pr), then);
            recheck_due(&view(&ready(), Some(&feat), false, &state), then + after)
        };
        let minute = Duration::from_secs(60);
        assert!(due(pr(PrState::Open, "remote", Some(ChecksState::Pending)), minute));
        assert!(due(pr(PrState::Draft, "remote", Some(ChecksState::Pending)), minute));
        // Just looked up by a full round.
        assert!(!due(pr(PrState::Open, "remote", Some(ChecksState::Pending)), Duration::from_secs(10)));
        assert!(!due(pr(PrState::Open, "remote", Some(ChecksState::Passing)), minute));
        assert!(!due(pr(PrState::Open, "remote", None), minute));
        assert!(!due(pr(PrState::Merged, "local", Some(ChecksState::Pending)), minute));
        // Failing already, but some still running.
        let mut failing = pr(PrState::Open, "remote", Some(ChecksState::Failing));
        failing.checks.as_mut().unwrap().pending = 2;
        assert!(due(failing, minute));

        // Still running half an hour on: only full rounds look again.
        let running = PrLookup {
            pr: Some(pr(PrState::Open, "remote", Some(ChecksState::Pending))),
            create_url: None,
        };
        let mut state = with_pr(key("feat", "remote"), running.pr.clone(), then);
        let later = then + 29 * minute;
        state.record(answer(key("feat", "remote"), Answer::Lookup(running.clone())), later);
        let due_at = |summary: &RepoSummary, state: &RepoPr, at| recheck_due(&view(&ready(), Some(summary), false, state), at);
        assert!(due_at(&feat, &state, later + Duration::from_secs(40)));
        assert!(!due_at(&feat, &state, later + minute));
        // A push starts them again.
        let mut pushed = on_branch("feat");
        pushed.remote_branch_oid = Some("pushed".into());
        state.record(answer(key("feat", "pushed"), Answer::Lookup(running)), later + minute);
        assert!(due_at(&pushed, &state, later + 2 * minute));
    }

    fn roots(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn start(names: &[&str], check_hosts: bool) -> Next {
        Next::Start {
            repos: roots(names),
            check_hosts,
        }
    }

    #[test]
    fn queue_debounces_changes_and_runs_one_lookup_at_a_time() {
        let t0 = Instant::now();
        let secs = |n| t0 + Duration::from_secs(n);
        let mut queue = PrQueue::default();
        assert_eq!(queue.next(&ready(), t0), Next::Idle);

        queue.changed("a".into(), t0);
        queue.changed("b".into(), secs(2));
        assert_eq!(queue.next(&ready(), secs(3)), Next::Wait(secs(7)));
        assert_eq!(queue.next(&ready(), secs(7)), start(&["a", "b"], false));
        // One at a time; changes meanwhile wait for the next.
        queue.changed("c".into(), secs(8));
        queue.now(roots(&["d"]));
        assert_eq!(queue.next(&ready(), secs(8)), Next::Idle);
        queue.finished();
        assert_eq!(queue.next(&ready(), secs(8)), start(&["c", "d"], false));
        queue.finished();
        assert_eq!(queue.next(&ready(), secs(9)), Next::Idle);

        // A stream of changes can only put it off so far.
        for n in (10..40).step_by(3) {
            queue.changed(format!("r{n}").into(), secs(n));
        }
        assert_eq!(queue.next(&ready(), secs(29)), Next::Wait(secs(30)));
        assert!(matches!(queue.next(&ready(), secs(30)), Next::Start { .. }));
    }

    #[test]
    fn queue_checks_gh_first_when_it_must() {
        let now = Instant::now();
        let mut queue = PrQueue::default();
        // First lookup.
        queue.now(roots(&["a"]));
        assert_eq!(queue.next(&GhStatus::Unchecked, now), start(&["a"], true));
        queue.finished();
        // Rounds ask for the hosts again.
        queue.round(roots(&["a", "b"]), &ready());
        assert_eq!(queue.next(&ready(), now), start(&["a", "b"], true));
        queue.finished();

        // Not logged in, or asking gh failed with no hosts to go on: changes are dropped,
        // since the next round looks up every repo anyway.
        for gh in [GhStatus::NotLoggedIn, GhStatus::Failed("timed out".into())] {
            queue.changed("a".into(), now);
            queue.now(roots(&["b"]));
            assert_eq!(queue.next(&gh, now), Next::Idle, "{gh:?}");
            queue.round(roots(&["a"]), &gh);
            assert_eq!(queue.next(&gh, now), start(&["a"], true), "{gh:?}");
            queue.finished();
        }

        // Not installed: rounds don't try again, `R` does.
        let a_while = Duration::from_secs(60);
        queue.round(roots(&["a"]), &GhStatus::NotInstalled);
        assert_eq!(queue.next(&GhStatus::NotInstalled, now), Next::Idle);
        assert!(queue.refresh(roots(&["a"]), a_while));
        assert_eq!(queue.next(&GhStatus::NotInstalled, now), start(&["a"], true));
        queue.finished();
        // `R` with no repos still checks gh.
        assert!(queue.refresh(Vec::new(), a_while));
        assert_eq!(queue.next(&GhStatus::NotInstalled, now), start(&[], true));
        queue.finished();

        assert!(queue.refresh(roots(&["a"]), a_while));
        assert_eq!(queue.next(&GhStatus::Disabled, now), Next::Idle);
        assert_eq!(queue.next(&ready(), now), Next::Idle);
    }

    #[test]
    fn pressing_r_again_makes_no_second_round() {
        let now = Instant::now();
        let a_while = Duration::from_secs(60);
        let mut queue = PrQueue::default();
        // Right after a round.
        assert!(!queue.refresh(roots(&["a"]), Duration::from_secs(2)));
        assert_eq!(queue.next(&ready(), now), Next::Idle);
        // While one is queued, then while it runs.
        assert!(queue.refresh(roots(&["a"]), a_while));
        assert!(!queue.refresh(roots(&["a", "b"]), a_while));
        assert_eq!(queue.next(&ready(), now), start(&["a"], true));
        assert!(!queue.refresh(roots(&["a", "b"]), a_while));
        queue.finished();
        assert!(queue.refresh(roots(&["a", "b"]), a_while));
        // A lookup that doesn't check gh doesn't hold `R` up.
        let mut queue = PrQueue::default();
        queue.now(roots(&["a"]));
        assert_eq!(queue.next(&ready(), now), start(&["a"], false));
        assert!(queue.refresh(roots(&["a", "b"]), a_while));
    }

    // `look_up` against real repos and a stand-in gh.

    fn git() -> Git {
        Git::new(Arc::new(())).with_env([("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")])
    }

    /// A repo whose `origin` has `url`.
    fn repo_with_origin(dir: &Path, name: &str, url: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        block_on(git().write(&root, ["init", "-q"])).unwrap();
        block_on(git().write(&root, ["remote", "add", "origin", url])).unwrap();
        root
    }

    fn request(root: &Path, branch: &str) -> Request {
        Request {
            root: root.to_path_buf(),
            key: key(branch, "remote"),
            local_oid: Some("local".into()),
            remote: None,
        }
    }

    /// A stand-in gh: `auth status` answers `hosts`, `api graphql` that each repo exists
    /// without PRs, or that it isn't logged in to a host listed in `logged_out`. Every
    /// call's arguments go to `calls`.
    fn fake_gh(dir: &Path, hosts: &str) -> Gh {
        let path = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{calls}"
if [ "$1" = api ] && grep -qx "$4" "{logged_out}" 2>/dev/null; then
  cat >/dev/null; echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4
fi
case "$1" in
  auth) echo '{{"hosts":{hosts}}}' ;;
  api)
    n=$(( $(cat | grep -o '"n[0-9]*":' | wc -l) ))
    printf '{{"data":{{'
    i=0
    while [ "$i" -lt "$n" ]; do
      [ "$i" -gt 0 ] && printf ','
      printf '"r%d":{{"nameWithOwner":"o/r","defaultBranchRef":{{"name":"main"}},"pullRequests":{{"nodes":[]}},"parent":null}}' "$i"
      i=$((i + 1))
    done
    printf '}}}}' ;;
esac
"#,
            calls = dir.join("calls").display(),
            logged_out = dir.join("logged_out").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh::new(Arc::new(())).with_program(path)
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    const LOGGED_IN: &str = r#"{"github.com":[{"active":true,"host":"github.com","login":"me","state":"success"}]}"#;

    #[test]
    fn looks_up_github_repos_once_per_host_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), LOGGED_IN);
        let github = repo_with_origin(dir.path(), "github", "git@github.com:o/r.git");
        // Shaped like a GitHub repo, but gh has no login there: never asked.
        let gitlab = repo_with_origin(dir.path(), "gitlab", "https://gitlab.com/o/r.git");
        let ghe = repo_with_origin(dir.path(), "ghe", "https://ghe.example.com/o/r.git");
        let local = repo_with_origin(dir.path(), "local", "/srv/git/r.git");
        // `get-url` applies `insteadOf`.
        let short = repo_with_origin(dir.path(), "short", "gh:o/r");
        block_on(git().write(&short, ["config", "url.https://github.com/.insteadOf", "gh:"])).unwrap();
        // The branch is pushed to a fork: the PR's head is there, not upstream.
        let fork = repo_with_origin(dir.path(), "fork", "https://github.com/upstream/r.git");
        block_on(git().write(&fork, ["remote", "set-url", "--push", "origin", "git@github.com:me/r.git"])).unwrap();
        // Fetched from a mirror, pushed to GitHub through `pushInsteadOf`.
        let mirror = repo_with_origin(dir.path(), "mirror", "https://mirror.example.org/o/r.git");
        block_on(git().write(&mirror, ["config", "url.git@github.com:.pushInsteadOf", "https://mirror.example.org/"])).unwrap();
        let requests = vec![
            request(&github, "feat"),
            request(&gitlab, "feat"),
            request(&ghe, "feat"),
            request(&local, "feat"),
            request(&short, "fix"),
            request(&fork, "feat"),
            request(&mirror, "feat"),
        ];

        let found = block_on(look_up(&git(), &gh, dir.path(), true, None, requests));
        assert_eq!(found.gh, Some(ready()));
        assert_eq!(calls(dir.path()), "auth status --json hosts\napi graphql --hostname github.com --input -\n");
        let answers: Vec<&Answer> = found.repos.iter().map(|r| &r.answer).collect();
        let create = |branch: &str| {
            Answer::Lookup(PrLookup {
                pr: None,
                create_url: Some(format!("https://github.com/o/r/compare/main...o:{branch}?expand=1")),
            })
        };
        let not_github = &Answer::NotOnGitHub;
        let feat = &create("feat");
        assert_eq!(answers, [feat, not_github, not_github, not_github, &create("fix"), feat, feat]);
        let remotes: Vec<Option<Option<RemoteRepo>>> = found.repos.iter().map(|r| r.remote.clone()).collect();
        let fork_repo = RemoteRepo {
            owner: "me".into(),
            ..github_repo("github.com")
        };
        assert_eq!(
            remotes,
            [
                Some(Some(github_repo("github.com"))),
                Some(Some(github_repo("gitlab.com"))),
                Some(Some(github_repo("ghe.example.com"))),
                Some(None),
                Some(Some(github_repo("github.com"))),
                Some(Some(fork_repo)),
                Some(Some(github_repo("github.com"))),
            ]
        );
    }

    #[test]
    fn looks_up_known_remotes_without_git_and_known_hosts_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), LOGGED_IN);
        // No repo there: git would fail.
        let request = Request {
            remote: Some(Some(github_repo("github.com"))),
            ..request(&dir.path().join("gone"), "feat")
        };
        let hosts = Some(vec!["github.com".to_string()]);
        let found = block_on(look_up(&git(), &gh, dir.path(), false, hosts, vec![request]));
        assert_eq!(found.gh, None);
        assert_eq!(calls(dir.path()), "api graphql --hostname github.com --input -\n");
        assert!(matches!(found.repos[0].answer, Answer::Lookup(_)));
    }

    #[test]
    fn asks_nothing_without_a_login() {
        let dir = tempfile::tempdir().unwrap();
        let github = repo_with_origin(dir.path(), "github", "https://github.com/o/r");
        // A URL that names no GitHub repo says so whatever gh's state.
        let gitlab = repo_with_origin(dir.path(), "gitlab", "https://gitlab.com/group/sub/r.git");
        let requests = || vec![request(&github, "feat"), request(&gitlab, "feat")];
        let not_logged_in = r#"{"github.com":[{"active":true,"host":"github.com","login":"me","state":"error"}]}"#;
        let gh = fake_gh(dir.path(), not_logged_in);
        let found = block_on(look_up(&git(), &gh, dir.path(), true, None, requests()));
        assert_eq!(found.gh, Some(GhStatus::NotLoggedIn));
        assert_eq!(found.repos[0].answer, Answer::NotAsked);
        assert_eq!(found.repos[1].answer, Answer::NotOnGitHub);
        assert_eq!(calls(dir.path()), "auth status --json hosts\n");

        let missing = Gh::new(Arc::new(())).with_program(dir.path().join("no-such-gh"));
        let found = block_on(look_up(&git(), &missing, dir.path(), true, None, requests()));
        let status = found.gh.clone().unwrap();
        assert_eq!(status, GhStatus::NotInstalled);
        let answers: Vec<&Answer> = found.repos.iter().map(|r| &r.answer).collect();
        assert_eq!(answers, [&Answer::NotAsked, &Answer::NotOnGitHub]);
        // What shows: gh's state for the GitHub repo, "not on GitHub" for the other.
        let shown: Vec<String> = found
            .repos
            .into_iter()
            .map(|answered| {
                let mut state = RepoPr::default();
                state.record(answered, SystemTime::now());
                format!("{:?}", view(&status, Some(&on_branch("feat")), false, &state))
            })
            .collect();
        assert_eq!(shown, ["GhNotInstalled", "Skipped(NotOnGitHub)"]);
    }

    #[test]
    fn a_host_gh_is_logged_out_of_leaves_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let both = r#"{"ghe.example.com":[{"active":true,"host":"ghe.example.com","login":"me","state":"success"}],
            "github.com":[{"active":true,"host":"github.com","login":"me","state":"success"}]}"#;
        let gh = fake_gh(dir.path(), both);
        // gh auth status says the login works, but GitHub turns it down.
        std::fs::write(dir.path().join("logged_out"), "ghe.example.com\n").unwrap();
        let ghe = repo_with_origin(dir.path(), "ghe", "https://ghe.example.com/o/r");
        let github = repo_with_origin(dir.path(), "github", "https://github.com/o/r");
        let requests = || vec![request(&ghe, "feat"), request(&github, "feat")];

        let found = block_on(look_up(&git(), &gh, dir.path(), true, None, requests()));
        assert_eq!(
            calls(dir.path()),
            "auth status --json hosts\n\
             api graphql --hostname ghe.example.com --input -\n\
             api graphql --hostname github.com --input -\n"
        );
        assert_eq!(found.gh, Some(ready()));
        assert_eq!(found.repos[0].answer, Answer::Failed("gh isn't logged in to ghe.example.com".into()));
        assert!(matches!(found.repos[1].answer, Answer::Lookup(_)));

        // Out of every host.
        std::fs::write(dir.path().join("logged_out"), "ghe.example.com\ngithub.com\n").unwrap();
        let hosts = Some(vec!["ghe.example.com".to_string(), "github.com".to_string()]);
        let found = block_on(look_up(&git(), &gh, dir.path(), false, hosts, requests()));
        assert_eq!(found.gh, Some(GhStatus::NotLoggedIn));
        assert_eq!(found.repos[1].answer, Answer::Failed("gh isn't logged in to github.com".into()));
    }

    #[test]
    fn a_failed_call_fails_its_repos() {
        let dir = tempfile::tempdir().unwrap();
        let github = repo_with_origin(dir.path(), "github", "https://github.com/o/r");
        let path = dir.path().join("gh");
        let script = "#!/bin/sh\ncat >/dev/null; echo 'gh: Something went wrong' >&2; exit 1\n";
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gh = Gh::new(Arc::new(())).with_program(path);

        // The host check fails too, but the hosts from last time still work.
        let hosts = Some(vec!["github.com".to_string()]);
        let found = block_on(look_up(&git(), &gh, dir.path(), true, hosts, vec![request(&github, "feat")]));
        assert_eq!(found.gh, None);
        assert_eq!(found.repos[0].answer, Answer::Failed("gh: Something went wrong".into()));
        // Without them, there's nothing to go on.
        let found = block_on(look_up(&git(), &gh, dir.path(), true, None, vec![request(&github, "feat")]));
        assert_eq!(found.gh, Some(GhStatus::Failed("gh: Something went wrong".into())));
        assert_eq!(found.repos[0].answer, Answer::NotAsked);
    }
}
