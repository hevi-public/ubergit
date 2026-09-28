//! Every open pull request of the workdir's repos, teammates' included, for the overview:
//! each repo's list, the PRs waiting on the user's review, and which local worktree or
//! branch each PR's head is checked out in. The per-branch lookup ([`crate::pr_status`])
//! stays as it is; this one asks about repos, not branches.
//!
//! A call asks about [`MAX_REPOS_PER_QUERY`] repos. With 30 PRs each and only their checks'
//! rollup state, a call of 40 measured `rateLimit { cost: 12, nodeCount: 2400 }` (asking
//! for the parent's PRs too, to resolve forks in the same call, measured 25). A workdir of
//! ~120 repos takes 3 calls, 36 points a round, and the review-requested search 1 more per
//! host: with rounds every 5 minutes, ~450 of GitHub's 5,000 points an hour, next to the
//! per-branch lookup's ~600.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::gh::{Gh, GhError};
use crate::git::Git;
use crate::github::{self, Answer, Connection, Failed, Login, MAX_REPOS_PER_QUERY, RemoteRepo, RepoName};
use crate::model::{ChecksState, ReviewDecision};
use crate::pr_status::GhStatus;
use crate::summary::primary_remote;

/// How many of a repo's open PRs are asked for, most recently updated first. The overview
/// says how many more there are.
pub const PRS_PER_REPO: usize = 30;
/// The selected repo's list is asked for again when it's selected and older than this.
pub const SELECTED_MAX_AGE: Duration = Duration::from_secs(60);
/// A push shows on GitHub, and starts its checks, a moment later.
pub const AFTER_PUSH: Duration = Duration::from_secs(5);
/// GitHub's search for the PRs asking for the user's review, their teams' included. Sorted
/// explicitly: the search covers the whole host, so [`REVIEW_RESULTS`] is a cut, and
/// GitHub's default relevance order would make which PRs fall inside it vary between rounds.
pub const REVIEW_SEARCH: &str = "is:open is:pr review-requested:@me archived:false sort:updated-desc";
/// Review requests asked for per host. Only those in the workdir's repos show. A request
/// past this cut still shows, under whoever wrote it rather than as waiting on the user.
const REVIEW_RESULTS: usize = 50;

/// An open pull request, as the overview lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `None` for a deleted account.
    pub author: Option<String>,
    pub draft: bool,
    pub review: Option<ReviewDecision>,
    /// The head commit's checks, as a whole; their names cost a query of their own.
    pub checks: Option<ChecksState>,
    pub updated: Option<SystemTime>,
    /// The branch it comes from, in `head_repo`.
    pub head_ref: String,
    /// `owner/name`; `None` once the head fork is deleted.
    pub head_repo: Option<String>,
}

/// A repo's open PRs, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoPrs {
    /// GitHub's `owner/name` of the repo listed: for a fork, its parent's.
    pub repo: String,
    /// For a fork, its own `owner/name`: the team's PRs are on the parent.
    pub fork: Option<String>,
    pub prs: Vec<OpenPr>,
    /// Open PRs in all, of which `prs` are the first [`PRS_PER_REPO`].
    pub total: u32,
}

/// A PR asking for the user's review, anywhere on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRequest {
    /// `owner/name` of the repo it's on.
    pub repo: String,
    pub pr: OpenPr,
}

/// A repo's open PRs on one host, and who's logged in there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPrs {
    pub viewer: Option<String>,
    /// One per repo asked about, in order.
    pub repos: Vec<Result<RepoPrs, String>>,
}

// ---- the queries ----------------------------------------------------------------------------

/// What each PR's line needs. Only the checks' rollup state: the checks themselves cost a
/// node each.
const PR_FIELDS: &str = "\
fragment OpenPr on PullRequest {
  number title url isDraft reviewDecision updatedAt headRefName
  author { login } headRepository { nameWithOwner }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}
";

/// The parent only by name: asking for its PRs here as well would double every call's
/// cost for the few repos that are forks. [`open_prs`] asks for those in a second call.
fn repo_fragment() -> String {
    format!(
        "fragment Open on Repository {{ nameWithOwner parent {{ nameWithOwner }} \
         pullRequests(states: OPEN, first: {PRS_PER_REPO}, orderBy: {{field: UPDATED_AT, direction: DESC}}) \
         {{ totalCount nodes {{ ...OpenPr }} }} }}\n"
    )
}

/// The body asking for each repo's open PRs, `r0`, `r1`... in order, and with `viewer`,
/// who's logged in. `repos` must not be empty. Owners and names only go in as variables.
pub fn build_open_query(repos: &[RemoteRepo], viewer: bool) -> Value {
    let mut declarations = Vec::with_capacity(repos.len());
    let mut selection = String::new();
    let mut variables = serde_json::Map::new();
    if viewer {
        selection.push_str("  viewer { login }\n");
    }
    for (i, repo) in repos.iter().enumerate() {
        // GitHub rejects the whole query if any declared variable goes unused.
        declarations.push(format!("$o{i}: String!, $n{i}: String!"));
        selection.push_str(&format!("  r{i}: repository(owner: $o{i}, name: $n{i}) {{ ...Open }}\n"));
        variables.insert(format!("o{i}"), repo.owner.clone().into());
        variables.insert(format!("n{i}"), repo.name.clone().into());
    }
    let query = format!(
        "query({}) {{\n{selection}}}\n{}{PR_FIELDS}",
        declarations.join(", "),
        repo_fragment()
    );
    json!({ "query": query, "variables": variables })
}

/// The body searching for the open PRs that ask for the user's review ([`REVIEW_SEARCH`]).
pub fn build_review_query() -> Value {
    let query = format!(
        "query($q: String!) {{\n  r0: search(query: $q, type: ISSUE, first: {REVIEW_RESULTS}) \
         {{ nodes {{ ... on PullRequest {{ ...OpenPr repository {{ nameWithOwner }} }} }} }}\n}}\n{PR_FIELDS}"
    );
    json!({ "query": query, "variables": { "q": REVIEW_SEARCH } })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoNode {
    name_with_owner: String,
    #[serde(default)]
    parent: Option<RepoName>,
    #[serde(default)]
    pull_requests: Connection<PrNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    is_draft: bool,
    review_decision: Option<String>,
    updated_at: Option<String>,
    head_ref_name: String,
    author: Option<Login>,
    head_repository: Option<RepoName>,
    #[serde(default)]
    commits: Connection<RollupCommit>,
}

#[derive(Deserialize)]
struct RollupCommit {
    commit: RollupOf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollupOf {
    status_check_rollup: Option<RollupState>,
}

#[derive(Deserialize)]
struct RollupState {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchNode {
    #[serde(flatten)]
    pr: PrNode,
    repository: RepoName,
}

impl PrNode {
    fn open_pr(&self) -> OpenPr {
        let checks = self
            .commits
            .nodes
            .last()
            .and_then(|c| c.commit.status_check_rollup.as_ref())
            .and_then(|rollup| github::checks_state(&rollup.state));
        OpenPr {
            number: self.number,
            title: self.title.clone(),
            url: self.url.clone(),
            author: self.author.as_ref().map(|a| a.login.clone()),
            draft: self.is_draft,
            review: github::review_decision(self.review_decision.as_deref()),
            checks,
            updated: self.updated_at.as_deref().and_then(parse_time),
            head_ref: self.head_ref_name.clone(),
            head_repo: self.head_repository.as_ref().map(|r| r.name_with_owner.clone()),
        }
    }
}

impl RepoNode {
    fn list(&self, fork: Option<String>) -> RepoPrs {
        RepoPrs {
            repo: self.name_with_owner.clone(),
            fork,
            total: self.pull_requests.total_count,
            prs: self.pull_requests.nodes.iter().map(PrNode::open_pr).collect(),
        }
    }
}

/// Reads the answer to [`build_open_query`] for `count` repos: who's logged in, when asked,
/// and each repo, or why GitHub gave none. An error anywhere in a repo's answer fails just
/// that repo, as in [`github::parse_response`].
fn read_open(mut answer: Answer, count: usize, viewer: bool) -> (Option<String>, Vec<Result<RepoNode, String>>) {
    let viewer = viewer.then(|| answer.take::<Login>("viewer").ok().map(|v| v.login)).flatten();
    (viewer, (0..count).map(|i| answer.take(&format!("r{i}"))).collect())
}

/// Reads the answer to [`build_review_query`].
pub fn parse_review(stdout: &[u8]) -> Result<Vec<ReviewRequest>, String> {
    read_review(Answer::parse(stdout)?)
}

fn read_review(mut answer: Answer) -> Result<Vec<ReviewRequest>, String> {
    let found: Connection<Value> = answer.take("r0")?;
    // Anything but a PR comes back as `{}`; `is:pr` should leave none.
    Ok(found
        .nodes
        .into_iter()
        .filter_map(|node| serde_json::from_value::<SearchNode>(node).ok())
        .map(|node| ReviewRequest {
            repo: node.repository.name_with_owner,
            pr: node.pr.open_pr(),
        })
        .collect())
}

/// Asks about `repos` in one call. One repo with thousands of PRs can make the whole call
/// time out, taking the others with it, so a call that fails is tried again as two halves,
/// once. Only gh missing or logged out is a [`GhError`].
async fn ask_open(
    gh: &Gh,
    host: &str,
    repos: &[RemoteRepo],
    viewer: bool,
    cwd: &Path,
) -> Result<(Option<String>, Vec<Result<RepoNode, String>>), GhError> {
    let once = async |repos: &[RemoteRepo], viewer: bool| match github::ask(gh, host, &build_open_query(repos, viewer), cwd).await {
        Ok(answer) => Ok(Ok(read_open(answer, repos.len(), viewer))),
        Err(Failed::Gh(err)) => Err(err),
        Err(Failed::Call(message)) => Ok(Err(message)),
    };
    match once(repos, viewer).await? {
        Ok(answer) => Ok(answer),
        // One result per repo, always: `nodes` has to stay aligned with what was asked.
        Err(message) if repos.len() < 2 => Ok((None, repos.iter().map(|_| Err(message.clone())).collect())),
        Err(_) => {
            let mut viewer_login = None;
            let mut nodes = Vec::with_capacity(repos.len());
            let (first, second) = repos.split_at(repos.len() / 2);
            for half in [first, second] {
                match once(half, viewer && viewer_login.is_none()).await? {
                    Ok((login, found)) => {
                        viewer_login = viewer_login.or(login);
                        nodes.extend(found);
                    }
                    Err(message) => nodes.extend(half.iter().map(|_| Err(message.clone()))),
                }
            }
            Ok((viewer_login, nodes))
        }
    }
}

/// Every open PR of each repo on `host`, one result per repo in order: for a fork, its
/// parent's, since that's where the team's are. Repos named twice, in any case, are asked
/// about once. `host` must come from [`github::logged_in_hosts`]. Only gh missing or not
/// logged in fails the whole lookup; a failed call fails just its own repos.
pub async fn open_prs(gh: &Gh, host: &str, repos: &[RemoteRepo], cwd: &Path) -> Result<HostPrs, GhError> {
    debug_assert!(repos.iter().all(|r| r.host == host));
    let key = |owner: &str, name: &str| format!("{owner}/{name}").to_lowercase();
    let mut unique: Vec<RemoteRepo> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let slots: Vec<usize> = repos
        .iter()
        .map(|repo| {
            *seen.entry(key(&repo.owner, &repo.name)).or_insert_with(|| {
                unique.push(repo.clone());
                unique.len() - 1
            })
        })
        .collect();

    let mut viewer = None;
    let mut nodes: Vec<Result<RepoNode, String>> = Vec::with_capacity(unique.len());
    for chunk in unique.chunks(MAX_REPOS_PER_QUERY) {
        let (login, found) = ask_open(gh, host, chunk, viewer.is_none(), cwd).await?;
        viewer = viewer.or(login);
        nodes.extend(found);
    }

    // The forks' parents, unless they were asked about already.
    let answered: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| Some((node.as_ref().ok()?.name_with_owner.to_lowercase(), i)))
        .collect();
    let mut parents: Vec<RemoteRepo> = Vec::new();
    let mut parent_slot: HashMap<String, usize> = HashMap::new();
    for node in nodes.iter().flatten() {
        let Some(parent) = &node.parent else { continue };
        let lower = parent.name_with_owner.to_lowercase();
        if answered.contains_key(&lower) || parent_slot.contains_key(&lower) {
            continue;
        }
        let Some((owner, name)) = parent.name_with_owner.split_once('/') else { continue };
        parent_slot.insert(lower, parents.len());
        parents.push(RemoteRepo {
            host: host.to_string(),
            owner: owner.into(),
            name: name.into(),
        });
    }
    let mut parent_nodes: Vec<Result<RepoNode, String>> = Vec::with_capacity(parents.len());
    for chunk in parents.chunks(MAX_REPOS_PER_QUERY) {
        parent_nodes.extend(ask_open(gh, host, chunk, false, cwd).await?.1);
    }

    // A fork shows its parent's list.
    let lists: Vec<Result<RepoPrs, String>> = nodes
        .iter()
        .map(|node| {
            let node = node.as_ref().map_err(Clone::clone)?;
            let Some(parent) = &node.parent else {
                return Ok(node.list(None));
            };
            let lower = parent.name_with_owner.to_lowercase();
            let parent_node = match (answered.get(&lower), parent_slot.get(&lower)) {
                (Some(&j), _) => &nodes[j],
                (None, Some(&j)) => &parent_nodes[j],
                (None, None) => return Err(format!("no answer from GitHub about {}", parent.name_with_owner)),
            };
            let parent_node = parent_node.as_ref().map_err(Clone::clone)?;
            Ok(parent_node.list(Some(node.name_with_owner.clone())))
        })
        .collect();
    Ok(HostPrs {
        viewer,
        repos: slots.into_iter().map(|slot| lists[slot].clone()).collect(),
    })
}

/// The open PRs on `host` that ask for the user's review, or their teams'. Only gh missing
/// or logged out is a [`GhError`].
pub async fn review_requests(gh: &Gh, host: &str, cwd: &Path) -> Result<Result<Vec<ReviewRequest>, String>, GhError> {
    match github::ask(gh, host, &build_review_query(), cwd).await {
        Ok(answer) => Ok(read_review(answer)),
        Err(Failed::Gh(err)) => Err(err),
        Err(Failed::Call(message)) => Ok(Err(message)),
    }
}

/// `2026-09-26T21:48:24Z`, as GitHub gives times; fractions of a second are dropped.
pub fn parse_time(text: &str) -> Option<SystemTime> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let number = |part: Option<&str>| part?.parse::<i64>().ok();
    let mut date = date.split('-');
    let (year, month, day) = (number(date.next())?, number(date.next())?, number(date.next())?);
    let mut time = time.split(':');
    let (hour, minute) = (number(time.next())?, number(time.next())?);
    let second = number(time.next()?.split('.').next())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 in the proleptic Gregorian calendar (Howard Hinnant's
    // `days_from_civil`).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(secs).ok().map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

// ---- the local side ------------------------------------------------------------------------------

/// A remote and the GitHub repos its URLs name, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRemote {
    pub name: String,
    pub fetch: Option<RemoteRepo>,
    pub push: Option<RemoteRepo>,
}

/// A local branch, and the remote and branch there it tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalBranch {
    pub name: String,
    pub upstream: Option<(String, String)>,
}

/// What a repo's worktrees share: its remotes and local branches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalRepo {
    pub remotes: Vec<LocalRemote>,
    pub branches: Vec<LocalBranch>,
}

impl LocalRepo {
    /// The GitHub repo whose PRs are the repo's: the one its primary remote fetches from.
    /// A remote can fetch from the team's repo and push to a fork.
    pub fn github(&self) -> Option<&RemoteRepo> {
        let names: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
        let primary = primary_remote(&names, None)?;
        self.remotes.iter().find(|r| r.name == primary)?.fetch.as_ref()
    }
}

/// Reads a repo's remotes and branches: two quick reads, both from the refs and config its
/// worktrees share.
pub async fn read_local(git: &Git, root: &Path) -> Result<LocalRepo, String> {
    let remotes = git.read(root, ["remote", "-v"]).await.map_err(|err| err.to_string())?;
    let format = "--format=%(refname:lstrip=2)%1f%(upstream:remotename)%1f%(upstream:remoteref)";
    let branches = git
        .read(root, ["for-each-ref", format, "refs/heads"])
        .await
        .map_err(|err| err.to_string())?;
    Ok(LocalRepo {
        remotes: parse_remotes(&remotes.stdout_str()),
        branches: parse_branches(&branches.stdout_str()),
    })
}

/// `git remote -v`: `origin\thttps://github.com/o/r.git (fetch)`, then its push URL.
fn parse_remotes(text: &str) -> Vec<LocalRemote> {
    let mut remotes: Vec<LocalRemote> = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once('\t') else { continue };
        let Some((url, kind)) = rest.rsplit_once(' ') else { continue };
        let at = match remotes.iter().position(|r| r.name == name) {
            Some(at) => at,
            None => {
                remotes.push(LocalRemote {
                    name: name.to_string(),
                    fetch: None,
                    push: None,
                });
                remotes.len() - 1
            }
        };
        let repo = github::parse_remote_url(url);
        match kind {
            "(fetch)" => remotes[at].fetch = repo,
            "(push)" => remotes[at].push = repo,
            _ => {}
        }
    }
    remotes
}

fn parse_branches(text: &str) -> Vec<LocalBranch> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\x1f');
            let name = fields.next().filter(|name| !name.is_empty())?;
            let remote = fields.next().unwrap_or_default();
            let branch = fields.next().unwrap_or_default().strip_prefix("refs/heads/");
            // A local upstream's remote is `.`: no PR comes from it.
            let upstream = match branch {
                Some(branch) if !remote.is_empty() && remote != "." => Some((remote.to_string(), branch.to_string())),
                _ => None,
            };
            Some(LocalBranch {
                name: name.to_string(),
                upstream,
            })
        })
        .collect()
}

/// Where a PR's head is checked out locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkout<'a> {
    /// In this worktree (or the main checkout), by its name.
    Worktree(&'a str),
    /// A local branch has it, but no worktree has that branch checked out.
    Local,
}

/// Where `pr`'s head branch is checked out, going by `local`, the repo's branches, and
/// `checkouts`, each of its worktrees by name with its branch (the main checkout first). A
/// branch has the PR's head when it tracks that branch on a remote whose URL is the head
/// repo; one without an upstream, when it has the same name and would be pushed there.
pub fn checkout<'a>(pr: &OpenPr, local: &LocalRepo, checkouts: &[(&'a str, Option<&str>)]) -> Option<Checkout<'a>> {
    let head_repo = pr.head_repo.as_deref()?;
    // Split once: this runs for every branch of every repo, for every PR listed.
    let (head_owner, head_name) = head_repo.split_once('/')?;
    let is_head = |repo: &Option<RemoteRepo>| {
        repo.as_ref().is_some_and(|repo| {
            repo.owner.eq_ignore_ascii_case(head_owner) && repo.name.eq_ignore_ascii_case(head_name)
        })
    };
    let remote_is_head = |name: &str| {
        local
            .remotes
            .iter()
            .find(|remote| remote.name == name)
            .is_some_and(|remote| is_head(&remote.fetch) || is_head(&remote.push))
    };
    let names: Vec<String> = local.remotes.iter().map(|r| r.name.clone()).collect();
    let primary = primary_remote(&names, None);
    let branches: Vec<&str> = local
        .branches
        .iter()
        .filter(|branch| match &branch.upstream {
            Some((remote, name)) => *name == pr.head_ref && remote_is_head(remote),
            None => branch.name == pr.head_ref && primary.is_some_and(remote_is_head),
        })
        .map(|branch| branch.name.as_str())
        .collect();
    if branches.is_empty() {
        return None;
    }
    let worktree = checkouts
        .iter()
        .find(|(_, branch)| branch.is_some_and(|branch| branches.contains(&branch)));
    Some(worktree.map_or(Checkout::Local, |(name, _)| Checkout::Worktree(name)))
}

// ---- the inbox -------------------------------------------------------------------------------------

/// A repo's list, for [`inbox`]: `group` says which of the caller's repos it is.
#[derive(Clone, Copy, Debug)]
pub struct Listed<'a> {
    pub group: usize,
    pub host: &'a str,
    pub prs: &'a RepoPrs,
}

/// A PR in the inbox, and which of the caller's repos it's in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboxPr<'a> {
    pub group: usize,
    pub host: &'a str,
    /// `owner/name` of the repo it's on.
    pub repo: &'a str,
    pub pr: &'a OpenPr,
}

/// The open PRs across the workdir, grouped, each newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inbox<'a> {
    /// Asking for the user's review, or their teams'.
    pub review: Vec<InboxPr<'a>>,
    /// By the user.
    pub yours: Vec<InboxPr<'a>>,
    /// Everyone else's.
    pub teammates: Vec<InboxPr<'a>>,
}

/// Groups the open PRs of `listed` repos. `requests` are each host's review requests, of
/// which only those in listed repos count; they come first, and aren't repeated. Authors
/// are compared with `viewer`, the login on the PR's host. A repo listed twice (two
/// clones, or a fork and its parent) counts once, with its first group.
pub fn inbox<'a>(
    listed: &[Listed<'a>],
    requests: &[(&'a str, &'a [ReviewRequest])],
    viewer: impl Fn(&str) -> Option<&'a str>,
) -> Inbox<'a> {
    let mut repos: Vec<Listed<'a>> = Vec::new();
    let mut seen: HashSet<(&str, String)> = HashSet::new();
    for list in listed {
        if seen.insert((list.host, list.prs.repo.to_lowercase())) {
            repos.push(*list);
        }
    }
    let find = |host: &str, repo: &str| {
        repos
            .iter()
            .find(|list| list.host == host && list.prs.repo.eq_ignore_ascii_case(repo))
    };
    let mut inbox = Inbox::default();
    let mut asked: BTreeSet<(&str, String, u64)> = BTreeSet::new();
    for &(host, found) in requests {
        for request in found {
            let Some(list) = find(host, &request.repo) else { continue };
            if asked.insert((host, request.repo.to_lowercase(), request.pr.number)) {
                inbox.review.push(InboxPr {
                    group: list.group,
                    host,
                    repo: &list.prs.repo,
                    pr: &request.pr,
                });
            }
        }
    }
    for list in &repos {
        let me = viewer(list.host);
        for pr in &list.prs.prs {
            if asked.contains(&(list.host, list.prs.repo.to_lowercase(), pr.number)) {
                continue;
            }
            let item = InboxPr {
                group: list.group,
                host: list.host,
                repo: &list.prs.repo,
                pr,
            };
            let mine = me.is_some_and(|me| pr.author.as_deref().is_some_and(|author| author.eq_ignore_ascii_case(me)));
            if mine { inbox.yours.push(item) } else { inbox.teammates.push(item) }
        }
    }
    for group in [&mut inbox.review, &mut inbox.yours, &mut inbox.teammates] {
        // Newest first; stable, so ties keep GitHub's order.
        group.sort_by_key(|item| std::cmp::Reverse(item.pr.updated));
    }
    inbox
}

// ---- when to ask ---------------------------------------------------------------------------------

/// Whether a list last looked up at `checked` is old enough to ask for again when its repo
/// is selected.
pub fn stale(checked: Option<SystemTime>, now: SystemTime) -> bool {
    checked.is_none_or(|checked| now.duration_since(checked).unwrap_or_default() >= SELECTED_MAX_AGE)
}

/// What's waiting to be looked up. One lookup runs at a time; whatever is queued meanwhile
/// goes into the next.
#[derive(Debug, Default)]
pub struct OpenQueue {
    /// Every repo, and the review requests.
    round: bool,
    /// Single repos, by their main checkout.
    repos: BTreeMap<PathBuf, Queued>,
    running: bool,
}

/// When a queued repo's lookup is wanted, and the earliest it may run.
#[derive(Clone, Copy, Debug)]
struct Queued {
    /// The soonest anything asked for it.
    at: Instant,
    /// A push GitHub may not have seen yet: never look up before this.
    not_before: Option<Instant>,
}

impl Queued {
    fn due(&self) -> Instant {
        self.not_before.map_or(self.at, |floor| self.at.max(floor))
    }
}

/// What [`OpenQueue::next`] says to do.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenNext {
    /// Nothing, until more is queued or the running lookup finishes.
    Idle,
    /// Call again then.
    Wait(Instant),
    /// Start a lookup ([`refresh`]) and call [`OpenQueue::finished`] when it's done. `round`
    /// means every repo and the review requests; else just `repos`.
    Start { round: bool, repos: Vec<PathBuf> },
}

impl OpenQueue {
    /// A full round: every repo and the review requests. It covers any single repos queued.
    pub fn round(&mut self) {
        self.round = true;
    }

    /// One repo, wanted by `at`: selecting it wants an answer as soon as there is one, so
    /// the soonest request wins. It still can't run before a push's floor.
    pub fn repo(&mut self, root: PathBuf, at: Instant) {
        let queued = self.repos.entry(root).or_insert(Queued { at, not_before: None });
        queued.at = queued.at.min(at);
    }

    /// One repo, never before `at`: a push has to reach GitHub first. The floor is kept
    /// apart from the wanted time so that selecting the repo, before or after, can't drag
    /// the lookup in front of the push and consume the entry with it. Pushing twice keeps
    /// the later floor.
    pub fn repo_after(&mut self, root: PathBuf, at: Instant) {
        let queued = self.repos.entry(root).or_insert(Queued { at, not_before: Some(at) });
        queued.not_before = Some(queued.not_before.map_or(at, |floor| floor.max(at)));
    }

    /// What to do now. Nothing runs while gh can't be asked (`ready` false); what's queued
    /// waits for it.
    pub fn next(&mut self, ready: bool, now: Instant) -> OpenNext {
        if self.running || !ready {
            return OpenNext::Idle;
        }
        if std::mem::take(&mut self.round) {
            // A round asks about every repo as GitHub sees it now, so it covers the repos
            // already due. One queued for later isn't: a push GitHub hasn't seen yet needs
            // the lookup this round is too early for.
            self.repos.retain(|_, queued| queued.due() > now);
            self.running = true;
            return OpenNext::Start {
                round: true,
                repos: Vec::new(),
            };
        }
        let due: Vec<PathBuf> =
            self.repos.iter().filter(|(_, q)| q.due() <= now).map(|(root, _)| root.clone()).collect();
        if due.is_empty() {
            return match self.repos.values().map(Queued::due).min() {
                Some(at) => OpenNext::Wait(at),
                None => OpenNext::Idle,
            };
        }
        for root in &due {
            self.repos.remove(root);
        }
        self.running = true;
        OpenNext::Start {
            round: false,
            repos: due,
        }
    }

    pub fn finished(&mut self) {
        self.running = false;
    }
}

// ---- a lookup ----------------------------------------------------------------------------------

/// What a repo's list is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    NoRemote,
    /// Its primary remote's URL names no GitHub repo, or one on a host gh isn't logged in
    /// to; the repo when it names one.
    NotOnGitHub(Option<RemoteRepo>),
    Prs(RepoPrs),
    Failed(String),
    /// gh couldn't be asked.
    NotAsked,
}

/// The answer about one repo.
#[derive(Clone, Debug)]
pub struct OpenAnswer {
    pub root: PathBuf,
    /// Its remotes and branches, or why they couldn't be read.
    pub local: Result<LocalRepo, String>,
    pub listing: Listing,
}

/// What one host said besides the repos' lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAnswer {
    pub host: String,
    pub viewer: Option<String>,
    /// Only asked for by full rounds.
    pub review: Option<Result<Vec<ReviewRequest>, String>>,
}

#[derive(Clone, Debug)]
pub struct OpenLookups {
    /// Set when gh turned out not to be installed.
    pub gh: Option<GhStatus>,
    /// One per root, in order.
    pub repos: Vec<OpenAnswer>,
    pub hosts: Vec<HostAnswer>,
}

/// Looks up the open PRs of each repo in `roots` (main checkouts), and with `review`, the
/// review requests on each host that has any of them. Each repo's remotes and branches are
/// read first. gh is only run for `hosts`, those [`github::logged_in_hosts`] named, and in
/// `cwd`.
pub async fn refresh(git: &Git, gh: &Gh, cwd: &Path, hosts: &[String], roots: Vec<PathBuf>, review: bool) -> OpenLookups {
    let locals: Vec<Result<LocalRepo, String>> =
        futures::future::join_all(roots.iter().map(|root| read_local(git, root))).await;
    let mut listings: Vec<Listing> = vec![Listing::NotAsked; roots.len()];
    let mut by_host: BTreeMap<&str, Vec<(usize, RemoteRepo)>> = BTreeMap::new();
    for (i, local) in locals.iter().enumerate() {
        let Ok(local) = local else { continue };
        if local.remotes.is_empty() {
            listings[i] = Listing::NoRemote;
            continue;
        }
        match local.github() {
            Some(repo) if hosts.contains(&repo.host) => {
                let host = hosts.iter().find(|h| **h == repo.host).map(String::as_str).unwrap_or_default();
                by_host.entry(host).or_default().push((i, repo.clone()));
            }
            other => listings[i] = Listing::NotOnGitHub(other.cloned()),
        }
    }

    let mut status = None;
    let mut host_answers = Vec::new();
    for (host, repos) in by_host {
        let asked: Vec<RemoteRepo> = repos.iter().map(|(_, repo)| repo.clone()).collect();
        let found = match open_prs(gh, host, &asked, cwd).await {
            Ok(found) => found,
            Err(GhError::NotInstalled) => {
                status = Some(GhStatus::NotInstalled);
                break;
            }
            Err(err) => {
                let message = match err {
                    GhError::NotLoggedIn { .. } => format!("gh isn't logged in to {host}"),
                    err => err.to_string(),
                };
                for (i, _) in &repos {
                    listings[*i] = Listing::Failed(message.clone());
                }
                continue;
            }
        };
        for ((i, _), list) in repos.iter().zip(found.repos) {
            listings[*i] = match list {
                Ok(list) => Listing::Prs(list),
                Err(message) => Listing::Failed(message),
            };
        }
        let review = match review {
            false => None,
            true => match review_requests(gh, host, cwd).await {
                Ok(found) => Some(found),
                Err(GhError::NotInstalled) => {
                    // Keep the viewer this host already gave: without it every PR of the
                    // user's reads as a teammate's.
                    status = Some(GhStatus::NotInstalled);
                    host_answers.push(HostAnswer {
                        host: host.to_string(),
                        viewer: found.viewer,
                        review: None,
                    });
                    break;
                }
                Err(err) => Some(Err(err.to_string())),
            },
        };
        host_answers.push(HostAnswer {
            host: host.to_string(),
            viewer: found.viewer,
            review,
        });
    }
    let repos = roots
        .into_iter()
        .zip(locals)
        .zip(listings)
        .map(|((root, local), listing)| OpenAnswer { root, local, listing })
        .collect();
    OpenLookups {
        gh: status,
        repos,
        hosts: host_answers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_io::block_on;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    fn repo(owner: &str, name: &str) -> RemoteRepo {
        RemoteRepo {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// The `$name`s in GraphQL text.
    fn variables(text: &str) -> BTreeSet<&str> {
        text.split('$')
            .skip(1)
            .map(|rest| &rest[..rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len())])
            .collect()
    }

    /// Declared, used and given variables are the same, and every fragment defined is used.
    fn check_query(body: &Value) {
        let query = body["query"].as_str().unwrap();
        let (header, selection) = query.split_once('{').unwrap();
        let declared = variables(header);
        assert_eq!(variables(selection), declared, "{query}");
        let given = body["variables"].as_object().unwrap();
        assert_eq!(given.keys().map(String::as_str).collect::<BTreeSet<_>>(), declared);
        for fragment in query.split("fragment ").skip(1) {
            let name = fragment.split_whitespace().next().unwrap();
            assert!(query.contains(&format!("...{name}")), "{name} unused in {query}");
        }
    }

    #[test]
    fn queries_use_every_variable_and_fragment_they_declare() {
        let repos: Vec<RemoteRepo> = (0..12).map(|i| repo(&format!("acme{i}"), &format!("w\"{i}\" {{x}}"))).collect();
        for viewer in [false, true] {
            let body = build_open_query(&repos, viewer);
            check_query(&body);
            let query = body["query"].as_str().unwrap();
            assert_eq!(query.contains("viewer { login }"), viewer);
            assert!(!query.contains("acme") && !query.contains("{x}"));
            for (i, r) in repos.iter().enumerate() {
                assert!(query.contains(&format!("r{i}: repository(owner: $o{i}, name: $n{i}) {{ ...Open }}")));
                assert_eq!(body["variables"][format!("o{i}")], r.owner.as_str());
                assert_eq!(body["variables"][format!("n{i}")], r.name.as_str());
            }
            // The rollup's state only: no check names.
            assert!(query.contains("statusCheckRollup { state }") && !query.contains("contexts"));
            assert!(query.contains("pullRequests(states: OPEN, first: 30, orderBy: {field: UPDATED_AT, direction: DESC})"));
        }
        let review = build_review_query();
        check_query(&review);
        assert_eq!(review["variables"]["q"], REVIEW_SEARCH);
        let detail = github::build_detail_query("o/r", 7);
        check_query(&detail);
        assert_eq!(detail["variables"], json!({ "o": "o", "n": "r", "p": 7 }));
    }

    /// A PR as the `OpenPr` fragment returns it.
    fn pr_json(number: u64, author: &str, updated: &str, head: Option<&str>, rollup: Option<&str>) -> Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "url": format!("https://github.com/o/r/pull/{number}"),
            "isDraft": false,
            "reviewDecision": null,
            "updatedAt": updated,
            "headRefName": format!("feat-{number}"),
            "author": { "login": author },
            "headRepository": head.map(|name| json!({ "nameWithOwner": name })),
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": rollup.map(|state| json!({ "state": state })) } }] },
        })
    }

    fn repo_json(name: &str, parent: Option<&str>, total: u32, prs: Vec<Value>) -> Value {
        json!({
            "nameWithOwner": name,
            "parent": parent.map(|name| json!({ "nameWithOwner": name })),
            "pullRequests": { "totalCount": total, "nodes": prs },
        })
    }

    /// Part of a real answer (natter, gh 2.98), with a deleted author and a PR the login
    /// can't see (null).
    const NATTER: &str = r#"{"data":{"viewer":{"login":"hevi-public"},"r0":{"nameWithOwner":"hevi-public/natter","parent":null,"pullRequests":{"totalCount":9,"nodes":[{"number":200,"title":"Record per-sentence speech timing, with an always-on history file (natter#191)","url":"https://github.com/hevi-public/natter/pull/200","isDraft":true,"reviewDecision":null,"updatedAt":"2026-09-26T21:48:24Z","headRefName":"claude/speech-generation-latency-20120c","author":{"login":"hevi-claudebot"},"headRepository":{"nameWithOwner":"hevi-public/natter"},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}},{"number":190,"title":"Old","url":"https://github.com/hevi-public/natter/pull/190","isDraft":false,"reviewDecision":"APPROVED","updatedAt":"2026-09-20T08:00:00Z","headRefName":"x","author":null,"headRepository":null,"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}},null]}}}}"#;

    #[test]
    fn reads_a_repos_open_prs() {
        let (viewer, nodes) = read_open(Answer::parse(NATTER.as_bytes()).unwrap(), 1, true);
        assert_eq!(viewer.as_deref(), Some("hevi-public"));
        let list = nodes[0].as_ref().unwrap().list(None);
        assert_eq!((list.repo.as_str(), list.total, list.prs.len()), ("hevi-public/natter", 9, 2));
        let first = &list.prs[0];
        assert_eq!(
            (first.number, first.draft, first.checks, first.review),
            (200, true, Some(ChecksState::Failing), None)
        );
        assert_eq!(first.author.as_deref(), Some("hevi-claudebot"));
        assert_eq!(first.head_repo.as_deref(), Some("hevi-public/natter"));
        assert_eq!(first.head_ref, "claude/speech-generation-latency-20120c");
        assert_eq!(first.updated, parse_time("2026-09-26T21:48:24Z"));
        let old = &list.prs[1];
        assert_eq!((old.author.as_deref(), old.head_repo.as_deref(), old.checks), (None, None, None));
        assert_eq!(old.review, Some(ReviewDecision::Approved));
    }

    #[test]
    fn an_error_in_one_repo_fails_only_that_repo() {
        let body = json!({
            "data": {
                "r0": repo_json("o/a", None, 1, vec![pr_json(1, "me", "2026-01-01T00:00:00Z", Some("o/a"), None)]),
                "r1": null,
                "r2": { "nameWithOwner": "o/c", "parent": null, "pullRequests": null },
            },
            "errors": [
                { "path": ["r1"], "message": "Could not resolve to a Repository with the name 'o/b'." },
                { "path": ["r2", "pullRequests"], "message": "timedout" },
            ],
        });
        let (viewer, nodes) = read_open(Answer::parse(body.to_string().as_bytes()).unwrap(), 3, false);
        assert_eq!(viewer, None);
        assert!(nodes[0].is_ok());
        assert_eq!(nodes[1].as_ref().err().map(String::as_str), Some("Could not resolve to a Repository with the name 'o/b'."));
        assert_eq!(nodes[2].as_ref().err().map(String::as_str), Some("timedout"));
        assert!(Answer::parse(br#"{"errors":[{"message":"Variable $n1 is declared by anonymous query but not used"}]}"#).is_err());
    }

    #[test]
    fn reads_review_requests() {
        let mut node = pr_json(5, "alice", "2026-01-02T00:00:00Z", Some("team/api"), Some("PENDING"));
        node["repository"] = json!({ "nameWithOwner": "team/api" });
        let body = json!({ "data": { "r0": { "nodes": [node, {}, null] } } });
        let found = parse_review(body.to_string().as_bytes()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].repo.as_str(), found[0].pr.number, found[0].pr.checks), ("team/api", 5, Some(ChecksState::Pending)));
    }

    #[test]
    fn parses_githubs_times() {
        let at = |secs: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), at(0));
        assert_eq!(parse_time("2026-09-26T21:48:24Z"), at(1_790_459_304));
        assert_eq!(parse_time("2000-02-29T12:00:00.5Z"), at(951_825_600));
        for bad in ["", "2026-09-26", "2026-09-26T21:48:24", "2026-13-01T00:00:00Z", "1969-12-31T23:59:59Z", "x-y-zT1:2:3Z"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    /// A stand-in gh that answers its `n`th call with the file `answer<n>`, and saves what
    /// it was asked in `body<n>`.
    fn fake_gh(dir: &Path, answers: &[Value]) -> Gh {
        for (i, answer) in answers.iter().enumerate() {
            std::fs::write(dir.join(format!("answer{}", i + 1)), answer.to_string()).unwrap();
        }
        let script = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$dir/count"
echo "$*" >> "$dir/args"
cat > "$dir/body$n"
if [ -e "$dir/answer$n" ]; then cat "$dir/answer$n"; else echo 'gh: HTTP 502' >&2; exit 1; fi
"#;
        let path = dir.join("gh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh::new(Arc::new(())).with_program(path)
    }

    fn body(dir: &Path, n: usize) -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(format!("body{n}"))).unwrap()).unwrap()
    }

    #[test]
    fn a_fork_lists_its_parents_prs() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = pr_json(3, "bob", "2026-01-02T00:00:00Z", Some("team/tool"), Some("SUCCESS"));
        let answers = [
            json!({ "data": {
                "viewer": { "login": "me" },
                "r0": repo_json("me/tool", Some("team/tool"), 0, vec![]),
                "r1": repo_json("o/r", None, 0, vec![]),
                "r2": repo_json("me/lib", Some("o/r"), 0, vec![]),
            } }),
            json!({ "data": { "r0": repo_json("team/tool", None, 41, vec![theirs]) } }),
        ];
        let gh = fake_gh(dir.path(), &answers);
        // `Me/Tool` is the same repo as `me/tool`: asked once.
        let repos = [repo("me", "tool"), repo("o", "r"), repo("me", "lib"), repo("Me", "Tool")];
        let found = block_on(open_prs(&gh, "github.com", &repos, dir.path())).unwrap();

        assert_eq!(found.viewer.as_deref(), Some("me"));
        let first = body(dir.path(), 1);
        assert_eq!(first["variables"], json!({ "o0": "me", "n0": "tool", "o1": "o", "n1": "r", "o2": "me", "n2": "lib" }));
        // Only the parent not asked about already, and without the viewer again.
        let second = body(dir.path(), 2);
        assert_eq!(second["variables"], json!({ "o0": "team", "n0": "tool" }));
        assert!(!second["query"].as_str().unwrap().contains("viewer"));
        assert!(!dir.path().join("body3").exists());

        let fork = found.repos[0].as_ref().unwrap();
        assert_eq!((fork.repo.as_str(), fork.fork.as_deref(), fork.total), ("team/tool", Some("me/tool"), 41));
        assert_eq!(fork.prs[0].number, 3);
        assert_eq!(found.repos[3], found.repos[0]);
        // A fork of a repo listed anyway takes that answer.
        let lib = found.repos[2].as_ref().unwrap();
        assert_eq!((lib.repo.as_str(), lib.fork.as_deref()), ("o/r", Some("me/lib")));
        assert_eq!(found.repos[1].as_ref().unwrap().fork, None);
    }

    #[test]
    fn a_failed_call_is_tried_again_in_halves() {
        let dir = tempfile::tempdir().unwrap();
        // The first call fails (no answer1), then each half is asked on its own.
        let answers = [
            Value::Null,
            json!({ "data": { "viewer": { "login": "me" }, "r0": repo_json("o/a", None, 0, vec![]) } }),
            json!({ "data": { "r0": repo_json("o/b", None, 0, vec![]), "r1": repo_json("o/c", None, 0, vec![]) } }),
        ];
        let gh = fake_gh(dir.path(), &answers);
        std::fs::remove_file(dir.path().join("answer1")).unwrap();
        let repos = [repo("o", "a"), repo("o", "b"), repo("o", "c")];
        let found = block_on(open_prs(&gh, "github.com", &repos, dir.path())).unwrap();
        assert_eq!(found.viewer.as_deref(), Some("me"));
        assert!(found.repos.iter().all(Result::is_ok), "{:?}", found.repos);
        assert_eq!(body(dir.path(), 3)["variables"], json!({ "o0": "o", "n0": "b", "o1": "o", "n1": "c" }));

        // A half that fails again fails its repos, and only those.
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), &[Value::Null, answers[1].clone()]);
        std::fs::remove_file(dir.path().join("answer1")).unwrap();
        let found = block_on(open_prs(&gh, "github.com", &repos, dir.path())).unwrap();
        assert!(found.repos[0].is_ok());
        assert_eq!(found.repos[1..], [Err("gh: HTTP 502".to_string()), Err("gh: HTTP 502".to_string())]);
    }

    fn open_pr(number: u64, author: Option<&str>, updated: u64, head_repo: Option<&str>, head_ref: &str) -> OpenPr {
        OpenPr {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/o/r/pull/{number}"),
            author: author.map(Into::into),
            draft: false,
            review: None,
            checks: None,
            updated: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(updated)),
            head_ref: head_ref.into(),
            head_repo: head_repo.map(Into::into),
        }
    }

    fn list(repo: &str, prs: Vec<OpenPr>) -> RepoPrs {
        RepoPrs {
            repo: repo.into(),
            fork: None,
            total: prs.len() as u32,
            prs,
        }
    }

    #[test]
    fn groups_the_inbox_by_review_requested_yours_and_teammates() {
        let api = list(
            "team/api",
            vec![
                open_pr(1, Some("Me"), 10, None, "a"),
                open_pr(2, Some("bob"), 30, None, "b"),
                open_pr(3, Some("carol"), 20, None, "c"),
                open_pr(9, None, 5, None, "ghost"),
            ],
        );
        let web = list("team/web", vec![open_pr(4, Some("dave"), 40, None, "d"), open_pr(5, Some("me"), 50, None, "e")]);
        // The same repo again, as another clone: counted once.
        let again = api.clone();
        let ghe = list("corp/tool", vec![open_pr(6, Some("me"), 60, None, "f")]);
        let requests = [
            ReviewRequest { repo: "Team/Api".into(), pr: open_pr(3, Some("carol"), 20, None, "c") },
            // Not in the workdir.
            ReviewRequest { repo: "elsewhere/x".into(), pr: open_pr(7, Some("eve"), 70, None, "g") },
            // Beyond the repo's first 30: still shown.
            ReviewRequest { repo: "team/web".into(), pr: open_pr(8, Some("frank"), 35, None, "h") },
        ];
        let listed = [
            Listed { group: 0, host: "github.com", prs: &api },
            Listed { group: 1, host: "github.com", prs: &web },
            Listed { group: 2, host: "github.com", prs: &again },
            Listed { group: 3, host: "ghe.example.com", prs: &ghe },
        ];
        let viewer = |host: &str| Some(if host == "github.com" { "me" } else { "someone" });
        let inbox = inbox(&listed, &[("github.com", &requests)], viewer);
        let numbers = |group: &[InboxPr]| group.iter().map(|item| (item.group, item.pr.number)).collect::<Vec<_>>();
        assert_eq!(numbers(&inbox.review), [(1, 8), (0, 3)]);
        assert_eq!(numbers(&inbox.yours), [(1, 5), (0, 1)]);
        // On the other host, someone else is logged in.
        assert_eq!(numbers(&inbox.teammates), [(3, 6), (1, 4), (0, 2), (0, 9)]);
        assert_eq!(inbox.review[1].repo, "team/api");

        // Without a known login, nothing is the user's.
        let unknown = super::inbox(&listed[..1], &[], |_| None);
        assert!(unknown.yours.is_empty() && unknown.review.is_empty());
        assert_eq!(unknown.teammates.len(), 4);
    }

    fn local(remotes: &[(&str, &str, &str)], branches: &[(&str, Option<(&str, &str)>)]) -> LocalRepo {
        LocalRepo {
            remotes: remotes
                .iter()
                .map(|(name, fetch, push)| LocalRemote {
                    name: name.to_string(),
                    fetch: github::parse_remote_url(fetch),
                    push: github::parse_remote_url(push),
                })
                .collect(),
            branches: branches
                .iter()
                .map(|(name, upstream)| LocalBranch {
                    name: name.to_string(),
                    upstream: upstream.map(|(remote, branch)| (remote.to_string(), branch.to_string())),
                })
                .collect(),
        }
    }

    #[test]
    fn finds_the_worktree_or_branch_a_pr_is_checked_out_in() {
        let repo = local(
            &[
                // Fetches from the team's repo, pushes to a fork.
                ("origin", "https://github.com/team/api.git", "git@github.com:me/api.git"),
                ("carol", "https://github.com/carol/api", "https://github.com/carol/api"),
            ],
            &[
                ("main", Some(("origin", "main"))),
                ("fix", Some(("origin", "fix-login"))),
                ("review-carol", Some(("carol", "speedup"))),
                ("wip", None),
                ("idle", Some(("origin", "idle"))),
                ("other", Some(("carol", "other"))),
            ],
        );
        let checkouts = [("api", Some("main")), ("api-fix", Some("fix")), ("api-review", Some("review-carol")), ("detached", None)];
        let at = |head_repo: Option<&str>, head_ref: &str| checkout(&open_pr(1, None, 0, head_repo, head_ref), &repo, &checkouts);

        // Tracks the branch under another name, pushed to the fork.
        assert_eq!(at(Some("me/api"), "fix-login"), Some(Checkout::Worktree("api-fix")));
        assert_eq!(at(Some("Carol/API"), "speedup"), Some(Checkout::Worktree("api-review")));
        // Pushed without -u: the same name, on the primary remote.
        assert_eq!(at(Some("me/api"), "wip"), Some(Checkout::Local));
        assert_eq!(at(Some("team/api"), "idle"), Some(Checkout::Local));
        // Another fork's branch of the same name isn't this one.
        assert_eq!(at(Some("dave/api"), "fix-login"), None);
        assert_eq!(at(Some("team/api"), "other"), None);
        assert_eq!(at(Some("team/api"), "nothing-here"), None);
        // The head fork is gone.
        assert_eq!(at(None, "fix-login"), None);
        assert_eq!(repo.github(), Some(&github::parse_remote_url("https://github.com/team/api").unwrap()));
    }

    #[test]
    fn reads_remotes_and_branches() {
        let remotes = "origin\thttps://github.com/team/api.git (fetch)\norigin\tgit@github.com:me/api.git (push)\n\
                       local\t/srv/git/api.git (fetch)\nlocal\t/srv/git/api.git (push)\n";
        let read = parse_remotes(remotes);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].fetch, Some(repo("team", "api")));
        assert_eq!(read[0].push, Some(repo("me", "api")));
        assert_eq!((read[1].name.as_str(), &read[1].fetch), ("local", &None));
        let branches = parse_branches("main\x1forigin\x1frefs/heads/main\nfeat/x\x1f\x1f\ntracks-local\x1f.\x1frefs/heads/main\n");
        assert_eq!(
            branches,
            [
                LocalBranch { name: "main".into(), upstream: Some(("origin".into(), "main".into())) },
                LocalBranch { name: "feat/x".into(), upstream: None },
                LocalBranch { name: "tracks-local".into(), upstream: None },
            ]
        );
    }

    #[test]
    fn reads_a_real_repos_remotes_and_branches() {
        let git = Git::new(Arc::new(())).with_env([("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| block_on(git.write(root, args.iter().copied())).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"]);
        run(&["remote", "add", "origin", "https://github.com/o/r.git"]);
        run(&["branch", "feature"]);
        run(&["config", "branch.feature.remote", "origin"]);
        run(&["config", "branch.feature.merge", "refs/heads/feature-on-remote"]);
        let found = block_on(read_local(&git, root)).unwrap();
        assert_eq!(found.github(), Some(&repo("o", "r")));
        assert_eq!(
            found.branches,
            [
                LocalBranch { name: "feature".into(), upstream: Some(("origin".into(), "feature-on-remote".into())) },
                LocalBranch { name: "main".into(), upstream: None },
            ]
        );
    }

    #[test]
    fn queue_runs_one_lookup_at_a_time_and_a_round_covers_single_repos() {
        let t0 = Instant::now();
        let secs = |n| t0 + Duration::from_secs(n);
        let mut queue = OpenQueue::default();
        assert_eq!(queue.next(true, t0), OpenNext::Idle);

        // Nothing runs until gh can be asked; it waits.
        queue.round();
        assert_eq!(queue.next(false, t0), OpenNext::Idle);
        queue.repo("a".into(), t0);
        assert_eq!(queue.next(true, t0), OpenNext::Start { round: true, repos: vec![] });
        // One at a time.
        queue.repo("b".into(), t0);
        queue.repo("c".into(), secs(5));
        assert_eq!(queue.next(true, secs(1)), OpenNext::Idle);
        queue.finished();
        assert_eq!(queue.next(true, secs(1)), OpenNext::Start { round: false, repos: vec!["b".into()] });
        queue.finished();
        assert_eq!(queue.next(true, secs(2)), OpenNext::Wait(secs(5)));
        // Queued sooner: the sooner time counts.
        queue.repo("c".into(), secs(3));
        assert_eq!(queue.next(true, secs(3)), OpenNext::Start { round: false, repos: vec!["c".into()] });
        queue.finished();
        assert_eq!(queue.next(true, secs(9)), OpenNext::Idle);

        // A round covers the repos due by now, but not one queued for later: a push
        // GitHub hasn't seen yet still needs its own lookup after the round.
        queue.repo("d".into(), secs(9));
        queue.repo("e".into(), secs(14));
        queue.round();
        assert_eq!(queue.next(true, secs(9)), OpenNext::Start { round: true, repos: vec![] });
        queue.finished();
        assert_eq!(queue.next(true, secs(10)), OpenNext::Wait(secs(14)));
        assert_eq!(queue.next(true, secs(14)), OpenNext::Start { round: false, repos: vec!["e".into()] });
        queue.finished();
        assert_eq!(queue.next(true, secs(15)), OpenNext::Idle);

        // A push wants GitHub to have seen it, so it can only move a repo's lookup later.
        // Selecting the repo queues it for now; pushing must not keep that time.
        queue.repo("f".into(), secs(20));
        queue.repo_after("f".into(), secs(25));
        assert_eq!(queue.next(true, secs(20)), OpenNext::Wait(secs(25)));
        assert_eq!(queue.next(true, secs(25)), OpenNext::Start { round: false, repos: vec!["f".into()] });
        queue.finished();
        // Pushing twice waits for the second push, not the first.
        queue.repo_after("g".into(), secs(30));
        queue.repo_after("g".into(), secs(34));
        assert_eq!(queue.next(true, secs(30)), OpenNext::Wait(secs(34)));
        // Selecting it meanwhile still can't drag the lookup in front of the push.
        queue.repo("g".into(), secs(31));
        assert_eq!(queue.next(true, secs(31)), OpenNext::Wait(secs(34)));
        assert_eq!(queue.next(true, secs(34)), OpenNext::Start { round: false, repos: vec!["g".into()] });
        queue.finished();
        assert_eq!(queue.next(true, secs(40)), OpenNext::Idle);
    }

    #[test]
    fn a_list_is_stale_after_a_minute() {
        let now = SystemTime::now();
        assert!(stale(None, now));
        assert!(!stale(Some(now - Duration::from_secs(59)), now));
        assert!(stale(Some(now - SELECTED_MAX_AGE), now));
        // A clock that went back.
        assert!(!stale(Some(now + Duration::from_secs(10)), now));
    }
}
