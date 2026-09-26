//! Looks up the pull request and CI status for many repos' current branches through
//! `gh api graphql`, batched: one query asks about up to [`MAX_REPOS_PER_QUERY`] repos.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::gh::{Gh, GhCommand, GhError};
use crate::model::{Checks, ChecksState, PrState, PullRequest, ReviewDecision, Reviewer, ReviewerState};
use crate::summary::{primary_remote, split_upstream};

/// Repos asked about per GraphQL call, which is also the per-host batch size: a host with
/// more repos gets several calls, one after another. A call of 40 measured 28,800 nodes
/// (a query may reach 500,000) and cost 17 of the 5,000 rate-limit points an hour; the
/// cost follows from the query's shape, not from how many PRs there are. A workdir of
/// ~120 repos takes 3 calls per host per refresh. To cut that, raise this (checking the
/// query's `rateLimit { cost nodeCount }`) or run the chunks concurrently.
/// This is the only place the chunk size is set.
pub const MAX_REPOS_PER_QUERY: usize = 40;

/// A GitHub repository, as a git remote names it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteRepo {
    /// Lowercase, as gh names hosts.
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Reads a remote URL: `[user@]host:owner/repo`, or an `ssh://`, `https://`, `http://` or
/// `git://` URL, each with an optional `.git` and trailing `/`. An IPv6 host keeps its
/// brackets, as in a URL. `None` for local paths and anything that isn't exactly
/// `owner/repo`, like a GitLab subgroup.
pub fn parse_remote_url(url: &str) -> Option<RemoteRepo> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !["ssh", "https", "http", "git"].iter().any(|s| scheme.eq_ignore_ascii_case(s)) {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            // gh names hosts without a port.
            let host = match host.find(']') {
                Some(end) if host.starts_with('[') => &host[..=end],
                _ => host.split_once(':').map_or(host, |(host, _)| host),
            };
            (host, path)
        }
        None => {
            // scp-like `[user@]host:path`. As in git, a '/' before the first ':' makes it a
            // path, and the ':' that ends the host comes after an IPv6 host's brackets.
            if url.find('/').is_some_and(|slash| url.find(':').is_none_or(|colon| slash < colon)) {
                return None;
            }
            let host_at = url.find("@[").map_or(0, |at| at + 1);
            let host_end = if url[host_at..].starts_with('[') { host_at + url[host_at..].find(']')? } else { host_at };
            let (authority, path) = url.split_at(host_end + url[host_end..].find(':')?);
            // `host:/owner/repo` names the same repo; git and GitHub take it.
            let path = &path[1..];
            let path = path.strip_prefix('/').unwrap_or(path);
            (authority.rsplit_once('@').map_or(authority, |(_, host)| host), path)
        }
    };
    // Only an IPv6 host, in its brackets, has a ':' or a bracket.
    let ipv6 = host.len() > 2 && host.starts_with('[') && host.ends_with(']');
    let inner = if ipv6 { &host[1..host.len() - 1] } else { host };
    if inner.contains(['[', ']']) || (!ipv6 && host.contains(':')) {
        return None;
    }
    let mut host = host.to_ascii_lowercase();
    // GitHub's ssh over port 443.
    if host == "ssh.github.com" {
        host = "github.com".into();
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if host.is_empty() || owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(RemoteRepo {
        host,
        owner: owner.into(),
        name: name.into(),
    })
}

/// The remote a local branch's PR would come from, and the branch's name there: from its
/// upstream when it has one, else the same name on the primary remote (a branch not pushed
/// yet, or pushed without setting an upstream). `None` without remotes.
pub fn remote_branch(upstream: Option<&str>, remotes: &[String], local_branch: &str) -> Option<(String, String)> {
    let remote = primary_remote(remotes, upstream)?;
    let branch = match upstream.and_then(|upstream| split_upstream(remotes, upstream)) {
        Some((_, branch)) => branch,
        None => local_branch,
    };
    Some((remote.to_string(), branch.to_string()))
}

/// A branch to find the pull request for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrTarget {
    pub repo: RemoteRepo,
    /// The branch's name on the remote, which is what a PR's head names.
    pub branch: String,
    /// The local branch tip. A merged PR only counts when its head is this commit, so a
    /// branch name used again after an old PR merged doesn't show as merged.
    pub local_oid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrLookup {
    pub pr: Option<PullRequest>,
    /// Without a PR: GitHub's page for opening one, against the parent's default branch for
    /// a fork. A browser needs no prompt for it, unlike `gh pr create --web`. `None` for the
    /// default branch itself, or a repo without one. It doesn't know whether the branch
    /// has been pushed.
    pub create_url: Option<String>,
}

/// Arguments of every PR search. Closed PRs are never shown, so they aren't fetched to
/// take up the five places.
const PR_SEARCH: &str = "first: 5, states: [OPEN, MERGED], orderBy: {field: UPDATED_AT, direction: DESC}";

const FRAGMENTS: &str = "\
fragment Repo on Repository { nameWithOwner defaultBranchRef { name } }
fragment Pr on PullRequest {
  number title url state isDraft updatedAt reviewDecision headRefOid baseRefName
  headRepository { nameWithOwner } baseRepository { nameWithOwner }
  reviewRequests(first: 10) { nodes { requestedReviewer {
    ... on User { login } ... on Team { slug } ... on Bot { login } ... on Mannequin { login }
  } } }
  latestOpinionatedReviews(first: 10) { nodes { author { login } state } }
  commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 50) { totalCount nodes {
    __typename ... on CheckRun { name status conclusion } ... on StatusContext { context state }
  } } } } } }
}
";

/// The request body asking about every target, `r0`, `r1`... in order. `targets` must not
/// be empty. Owners, names and branches only go in as variables, never into the text.
///
/// Each asks the branch for its PRs, wherever they go: a fork's usually go to its parent.
/// Asking the parent for its PRs by head branch name instead would list every fork's
/// branch of that name (a popular repo has over a thousand `patch-1`s), crowding this one
/// out of the five. The repo's own PRs are asked for by head branch name as well, so one
/// whose branch was deleted after it merged still turns up. A fork's can't: once its
/// branch is deleted, it no longer shows as merged.
pub fn build_query(targets: &[PrTarget]) -> Value {
    let mut declarations = Vec::with_capacity(targets.len());
    let mut selection = String::new();
    let mut variables = serde_json::Map::new();
    for (i, target) in targets.iter().enumerate() {
        // GitHub rejects the whole query if any declared variable goes unused.
        declarations.push(format!("$o{i}: String!, $n{i}: String!, $b{i}: String!, $q{i}: String!"));
        let own = format!("pullRequests(headRefName: $b{i}, {PR_SEARCH}) {{ nodes {{ ...Pr }} }}");
        let branch =
            format!("ref(qualifiedName: $q{i}) {{ associatedPullRequests({PR_SEARCH}) {{ nodes {{ ...Pr }} }} }}");
        // The parent only for where to propose a PR (`create_url`).
        let _ = writeln!(
            selection,
            "  r{i}: repository(owner: $o{i}, name: $n{i}) {{ ...Repo {own} {branch} parent {{ ...Repo }} }}"
        );
        variables.insert(format!("o{i}"), target.repo.owner.clone().into());
        variables.insert(format!("n{i}"), target.repo.name.clone().into());
        variables.insert(format!("b{i}"), target.branch.clone().into());
        variables.insert(format!("q{i}"), format!("refs/heads/{}", target.branch).into());
    }
    let query = format!("query({}) {{\n{selection}}}\n{FRAGMENTS}", declarations.join(", "));
    json!({ "query": query, "variables": variables })
}

/// Reads `gh api graphql`'s answer to [`build_query`] for the same `targets`, one result
/// per target. Only a body without `data` fails as a whole; a repo GitHub couldn't
/// resolve (missing, or not visible to the login), or any error inside a repo's answer,
/// fails just its own target.
pub fn parse_response(stdout: &[u8], targets: &[PrTarget]) -> Result<Vec<Result<PrLookup, String>>, String> {
    let repos = parse_repos(stdout, targets.len())?;
    Ok(targets
        .iter()
        .zip(repos)
        .map(|(target, repo)| repo.map(|repo| resolve(&repo, target)))
        .collect())
}

/// Hosts gh has a working login for. Only ask gh about repos on these hosts: with
/// `GH_ENTERPRISE_TOKEN` set, gh sends that token to whatever host other than github.com
/// `--hostname` names, so asking about a remote on, say, a GitLab server would hand it over.
/// `cwd` is only where gh runs (the command log names it), e.g. the workdir.
pub async fn logged_in_hosts(gh: &Gh, cwd: &Path) -> Result<Vec<String>, GhError> {
    // With --json, gh exits 0 whatever state the logins are in.
    match gh.run(GhCommand::new(["auth", "status", "--json", "hosts"]).cwd(cwd)).await {
        Ok(out) => Ok(parse_auth_hosts(&out.stdout).unwrap_or_else(|| vec!["github.com".into()])),
        // An older gh without --json here. github.com never gets the enterprise token.
        Err(GhError::Failed { stderr, .. }) if stderr.contains("unknown flag") => Ok(vec!["github.com".into()]),
        Err(err) => Err(err),
    }
}

/// Looks up the PR for each target, one result per target in the same order. `host` must
/// come from [`logged_in_hosts`], and every target's repo must be on it. Targets with the
/// same repo and branch (two clones, say) are asked about once. Asks
/// [`MAX_REPOS_PER_QUERY`] repos per call. Only gh missing or not logged in fails the
/// whole lookup; any other failed call fails just its own repos, so the other calls'
/// answers still count. gh runs in `cwd`, as in [`logged_in_hosts`].
pub async fn lookup(
    gh: &Gh,
    host: &str,
    targets: &[PrTarget],
    cwd: &Path,
) -> Result<Vec<Result<PrLookup, String>>, GhError> {
    debug_assert!(targets.iter().all(|t| t.repo.host == host));
    let mut unique: Vec<PrTarget> = Vec::new();
    let mut seen: HashMap<(&RemoteRepo, &str), usize> = HashMap::new();
    let slots: Vec<usize> = targets
        .iter()
        .map(|t| {
            *seen.entry((&t.repo, t.branch.as_str())).or_insert_with(|| {
                unique.push(t.clone());
                unique.len() - 1
            })
        })
        .collect();

    let mut repos = Vec::with_capacity(unique.len());
    for chunk in unique.chunks(MAX_REPOS_PER_QUERY) {
        let cmd = GhCommand::new(["api", "graphql", "--hostname", host, "--input", "-"])
            .cwd(cwd)
            .stdin(build_query(chunk).to_string());
        let parsed = match gh.run(cmd).await {
            Ok(out) => parse_repos(&out.stdout, chunk.len()),
            // Every other call would fail the same way.
            Err(err @ (GhError::NotInstalled | GhError::NotLoggedIn { .. })) => return Err(err),
            Err(err) => partial_answer(&err, chunk.len()).ok_or_else(|| err.to_string()),
        };
        match parsed {
            Ok(parsed) => repos.extend(parsed),
            // A timeout, say, or gh exiting 0 with something other than an answer: the
            // other chunks can still work.
            Err(message) => repos.extend(chunk.iter().map(|_| Err(message.clone()))),
        }
    }
    Ok(targets
        .iter()
        .zip(slots)
        .map(|(target, slot)| repos[slot].as_ref().map(|repo| resolve(repo, target)).map_err(Clone::clone))
        .collect())
}

/// `gh api graphql` exits 1 when part of a query failed, but still prints the rest.
fn partial_answer(err: &GhError, count: usize) -> Option<Vec<Result<RepoNode, String>>> {
    match err {
        GhError::Failed { stdout, .. } => parse_repos(stdout, count).ok(),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Response {
    data: Option<serde_json::Map<String, Value>>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Deserialize)]
struct GraphqlError {
    #[serde(default)]
    message: String,
    /// Where it happened, starting with the alias: `["r1"]` for the repo, or deeper, like
    /// `["r1", "pullRequests"]`, for a field inside it.
    path: Option<Vec<Value>>,
}

/// The repo behind each alias `r0`..`r{count-1}`, or why GitHub gave none.
fn parse_repos(stdout: &[u8], count: usize) -> Result<Vec<Result<RepoNode, String>>, String> {
    let response: Response =
        serde_json::from_slice(stdout).map_err(|err| format!("unexpected answer from GitHub: {err}"))?;
    let errors = response.errors.unwrap_or_default();
    let Some(mut data) = response.data else {
        return Err(errors
            .first()
            .map_or_else(|| "GitHub's answer has no data".into(), |e| e.message.clone()));
    };
    let error_at = |alias: &str| {
        errors
            .iter()
            .find(|e| e.path.as_ref().and_then(|p| p.first()).and_then(Value::as_str) == Some(alias))
            .map(|e| e.message.clone())
    };
    Ok((0..count)
        .map(|i| {
            let alias = format!("r{i}");
            // GitHub nulls a field that failed and leaves the repo, so an error anywhere in
            // it would otherwise read as no PR.
            if let Some(message) = error_at(&alias) {
                return Err(message);
            }
            match data.remove(&alias) {
                None | Some(Value::Null) => Err("no answer from GitHub".into()),
                Some(repo) => serde_json::from_value(repo).map_err(|err| format!("unexpected answer from GitHub: {err}")),
            }
        })
        .collect())
}

fn resolve(repo: &RepoNode, target: &PrTarget) -> PrLookup {
    let pr = pick_pr(repo, target.local_oid.as_deref());
    let create_url = match pr {
        None => create_url(&target.repo.host, repo, &target.branch),
        Some(_) => None,
    };
    PrLookup { pr, create_url }
}

/// The PR to show for a branch, from the branch's PRs and the repo's PRs with that head
/// branch name: the most recently updated open one, else a merged one whose head is still
/// the local tip. Only PRs whose head is in this repo count, as GitHub names it: that
/// follows a renamed owner or a moved repo, which the remote's URL may not, and another
/// fork can have a branch of the same name.
fn pick_pr(repo: &RepoNode, local_oid: Option<&str>) -> Option<PullRequest> {
    let branch_prs = repo.branch.as_ref().map(|branch| &branch.associated_pull_requests.nodes);
    let mut candidates: Vec<(&PrNode, &str)> = Vec::new();
    for pr in repo.pull_requests.nodes.iter().chain(branch_prs.into_iter().flatten()) {
        // No head repo means the head fork was deleted: not ours any more.
        let ours = pr
            .head_repository
            .as_ref()
            .is_some_and(|head| head.name_with_owner.eq_ignore_ascii_case(&repo.name_with_owner));
        let base = pr
            .base_repository
            .as_ref()
            .map_or(repo.name_with_owner.as_str(), |base| base.name_with_owner.as_str());
        // A PR into the repo itself is in both lists.
        let seen = candidates.iter().any(|&(seen, seen_base)| seen.number == pr.number && seen_base == base);
        if ours && !seen {
            candidates.push((pr, base));
        }
    }
    // Each list is newest first; this interleaves them. Stable, so ties keep the repo's own.
    candidates.sort_by(|(a, _), (b, _)| b.updated_at.cmp(&a.updated_at));
    let (pr, base_repo) = candidates.iter().find(|(pr, _)| pr.state == "OPEN").or_else(|| {
        candidates
            .iter()
            .find(|(pr, _)| pr.state == "MERGED" && local_oid == Some(pr.head_ref_oid.as_str()))
    })?;

    Some(PullRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        state: match (pr.state.as_str(), pr.is_draft) {
            ("MERGED", _) => PrState::Merged,
            (_, true) => PrState::Draft,
            _ => PrState::Open,
        },
        review: match pr.review_decision.as_deref() {
            Some("APPROVED") => Some(ReviewDecision::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
            Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
            _ => None,
        },
        checks: pr
            .commits
            .nodes
            .last()
            .and_then(|c| c.commit.status_check_rollup.as_ref())
            .and_then(checks),
        reviewers: reviewers(pr),
        base_ref: pr.base_ref_name.clone(),
        base_repo: base_repo.to_string(),
        head_oid: pr.head_ref_oid.clone(),
    })
}

/// Check-run conclusions that count as failed; the others are success, neutral, skipped
/// and stale.
const FAILED_CONCLUSIONS: [&str; 5] = ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"];

fn checks(rollup: &Rollup) -> Option<Checks> {
    let state = match rollup.state.as_str() {
        "SUCCESS" => ChecksState::Passing,
        "FAILURE" | "ERROR" => ChecksState::Failing,
        "PENDING" | "EXPECTED" => ChecksState::Pending,
        _ => return None,
    };
    let mut failing: Vec<String> = Vec::new();
    let mut pending = 0;
    for context in &rollup.contexts.nodes {
        let (name, failed, waiting) = match context {
            RollupContext::CheckRun { name, status, conclusion } => (
                name,
                conclusion.as_deref().is_some_and(|c| FAILED_CONCLUSIONS.contains(&c)),
                status != "COMPLETED",
            ),
            RollupContext::StatusContext { context, state } => (
                context,
                matches!(state.as_str(), "FAILURE" | "ERROR"),
                matches!(state.as_str(), "PENDING" | "EXPECTED"),
            ),
            RollupContext::Other => continue,
        };
        if failed && !failing.contains(name) {
            failing.push(name.clone());
        }
        if waiting {
            pending += 1;
        }
    }
    Some(Checks {
        state,
        failing,
        total: rollup.contexts.total_count,
        pending,
    })
}

/// Everyone asked to review, then everyone who approved or requested changes and hasn't
/// been asked again since.
fn reviewers(pr: &PrNode) -> Vec<Reviewer> {
    let mut reviewers: Vec<Reviewer> = pr
        .review_requests
        .nodes
        .iter()
        .filter_map(|request| {
            let reviewer = request.requested_reviewer.as_ref()?;
            let name = reviewer.login.as_ref().or(reviewer.slug.as_ref())?;
            Some(Reviewer {
                name: name.clone(),
                state: ReviewerState::Requested,
            })
        })
        .collect();
    for review in &pr.latest_opinionated_reviews.nodes {
        let Some(author) = &review.author else {
            continue;
        };
        let state = match review.state.as_str() {
            "APPROVED" => ReviewerState::Approved,
            "CHANGES_REQUESTED" => ReviewerState::ChangesRequested,
            _ => continue,
        };
        if !reviewers.iter().any(|r| r.name.eq_ignore_ascii_case(&author.login)) {
            reviewers.push(Reviewer {
                name: author.login.clone(),
                state,
            });
        }
    }
    reviewers
}

fn create_url(host: &str, repo: &RepoNode, branch: &str) -> Option<String> {
    let base = repo.parent.as_deref().unwrap_or(repo);
    let default = &base.default_branch_ref.as_ref()?.name;
    // The default branch has nothing to propose to itself; a fork's can go to the parent.
    if repo.parent.is_none() && default == branch {
        return None;
    }
    let (owner, _) = repo.name_with_owner.split_once('/')?;
    Some(format!(
        "https://{host}/{}/compare/{}...{owner}:{}?expand=1",
        base.name_with_owner,
        encode_path(default),
        encode_path(branch)
    ))
}

/// Percent-encodes a branch name for a URL path. `/` stays, as in GitHub's own links.
fn encode_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `gh auth status --json hosts`: the hosts whose login works. gh uses a host's active
/// account, so that one decides when it's marked.
fn parse_auth_hosts(stdout: &[u8]) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Status {
        hosts: BTreeMap<String, Vec<Account>>,
    }
    #[derive(Deserialize)]
    struct Account {
        #[serde(default)]
        active: bool,
        state: String,
    }

    let status: Status = serde_json::from_slice(stdout).ok()?;
    let works = |account: &Account| account.state == "success";
    Some(
        status
            .hosts
            .into_iter()
            .filter(|(_, accounts)| match accounts.iter().find(|a| a.active) {
                Some(active) => works(active),
                None => accounts.iter().any(works),
            })
            .map(|(host, _)| host.to_ascii_lowercase())
            .collect(),
    )
}

// The shapes the query's selections come back in.

/// A GraphQL connection. GitHub makes the connection, its `nodes` and each node nullable
/// (a node the login can't see comes back null), so nulls read as absent.
struct Connection<T> {
    nodes: Vec<T>,
    total_count: u32,
}

impl<T> Default for Connection<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            total_count: 0,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Connection<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw<T> {
            total_count: Option<u32>,
            nodes: Option<Vec<Option<T>>>,
        }
        let raw = Option::<Raw<T>>::deserialize(deserializer)?;
        let (nodes, total_count) = raw.map_or((None, None), |raw| (raw.nodes, raw.total_count));
        Ok(Self {
            nodes: nodes.into_iter().flatten().flatten().collect(),
            total_count: total_count.unwrap_or(0),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoNode {
    /// GitHub's name for it, which follows renames, unlike the remote's URL.
    name_with_owner: String,
    /// `None` for an empty repo.
    default_branch_ref: Option<RefName>,
    /// Its PRs from a branch of the target's name, in any fork.
    #[serde(default)]
    pull_requests: Connection<PrNode>,
    /// The target's branch; `None` while it isn't on the remote.
    #[serde(default, rename = "ref")]
    branch: Option<BranchRef>,
    /// Set for a fork.
    #[serde(default)]
    parent: Option<Box<RepoNode>>,
}

#[derive(Deserialize)]
struct RefName {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchRef {
    /// The PRs from this branch, into whichever repo.
    #[serde(default)]
    associated_pull_requests: Connection<PrNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    title: String,
    url: String,
    /// OPEN, CLOSED or MERGED.
    state: String,
    #[serde(default)]
    is_draft: bool,
    /// ISO 8601 in UTC, so it sorts as text.
    updated_at: Option<String>,
    review_decision: Option<String>,
    head_ref_oid: String,
    base_ref_name: String,
    /// `None` once the head fork is deleted.
    head_repository: Option<RepoName>,
    base_repository: Option<RepoName>,
    #[serde(default)]
    review_requests: Connection<ReviewRequest>,
    #[serde(default)]
    latest_opinionated_reviews: Connection<Review>,
    #[serde(default)]
    commits: Connection<PrCommit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoName {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequest {
    /// Null when the login can't see the reviewer, as happens with teams.
    requested_reviewer: Option<RequestedReviewer>,
}

/// A user, bot or mannequin has a login; a team has a slug.
#[derive(Deserialize)]
struct RequestedReviewer {
    login: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize)]
struct Review {
    author: Option<Login>,
    state: String,
}

#[derive(Deserialize)]
struct PrCommit {
    commit: HeadCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadCommit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: String,
    #[serde(default)]
    contexts: Connection<RollupContext>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum RollupContext {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_io::block_on;
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::Arc;

    fn repo(host: &str, owner: &str, name: &str) -> RemoteRepo {
        RemoteRepo {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn target(owner: &str, name: &str, branch: &str, local_oid: Option<&str>) -> PrTarget {
        PrTarget {
            repo: repo("github.com", owner, name),
            branch: branch.into(),
            local_oid: local_oid.map(Into::into),
        }
    }

    #[test]
    fn parses_remote_urls() {
        let github = |owner, name| Some(repo("github.com", owner, name));
        let cases = [
            ("git@github.com:hevi-public/ubergit.git", github("hevi-public", "ubergit")),
            ("git@github.com:hevi-public/ubergit", github("hevi-public", "ubergit")),
            ("github.com:owner/repo", github("owner", "repo")),
            ("ssh://git@github.com:22/owner/repo.git", github("owner", "repo")),
            ("ssh://git@ssh.github.com:443/owner/repo.git", github("owner", "repo")),
            ("git@ssh.github.com:owner/repo.git", github("owner", "repo")),
            ("https://github.com/owner/repo", github("owner", "repo")),
            ("https://github.com/owner/repo.git", github("owner", "repo")),
            ("https://github.com/owner/repo/", github("owner", "repo")),
            ("https://github.com/owner/repo.git/", github("owner", "repo")),
            ("https://x-access-token:secret@GitHub.com/Owner/Repo.git", github("Owner", "Repo")),
            ("git://github.com/owner/repo.git", github("owner", "repo")),
            ("https://me@ghe.example.com:8443/team/tool", Some(repo("ghe.example.com", "team", "tool"))),
            ("http://ghe.example.com/team/tool.git", Some(repo("ghe.example.com", "team", "tool"))),
            ("git@ghe.example.com:team/tool.git", Some(repo("ghe.example.com", "team", "tool"))),
            // A leading '/' after the ':' names the same repo.
            ("git@github.com:/owner/repo.git", github("owner", "repo")),
            ("git@github.com:/srv/git/repo.git", None),
            // IPv6 hosts keep their brackets, so they still make a URL.
            ("git@[::1]:team/tool", Some(repo("[::1]", "team", "tool"))),
            ("[::1]:team/tool.git", Some(repo("[::1]", "team", "tool"))),
            ("ssh://git@[2001:DB8::1]:22/team/tool.git", Some(repo("[2001:db8::1]", "team", "tool"))),
            ("https://[2001:db8::1]/team/tool", Some(repo("[2001:db8::1]", "team", "tool"))),
            ("git@[::1:team/tool", None),
            ("ssh://git@[::1:22/team/tool", None),
            ("https://[]/team/tool", None),
            ("/srv/git/owner/repo.git", None),
            ("../owner/repo", None),
            ("./dir:owner/repo", None),
            ("file:///srv/git/owner/repo.git", None),
            ("https://gitlab.com/group/sub/repo.git", None),
            ("git@gitlab.com:group/sub/repo.git", None),
            ("https://github.com/owner", None),
            ("git@github.com:repo.git", None),
            ("https://github.com/owner/.git", None),
            ("https://github.com//repo", None),
            ("", None),
        ];
        for (url, want) in cases {
            assert_eq!(parse_remote_url(url), want, "{url}");
        }
    }

    #[test]
    fn remote_branch_follows_the_upstream_else_the_primary_remote() {
        let remotes = vec!["origin".to_string(), "team".to_string(), "team/x".to_string()];
        let pair = |remote: &str, branch: &str| Some((remote.to_string(), branch.to_string()));
        assert_eq!(remote_branch(Some("origin/feature-x"), &remotes, "local"), pair("origin", "feature-x"));
        assert_eq!(remote_branch(Some("team/x/fix/y"), &remotes, "local"), pair("team/x", "fix/y"));
        assert_eq!(remote_branch(Some("team/fix"), &remotes, "local"), pair("team", "fix"));
        assert_eq!(remote_branch(None, &remotes, "local"), pair("origin", "local"));
        // Tracking a local branch: no remote in its name.
        assert_eq!(remote_branch(Some("main"), &remotes, "local"), pair("origin", "local"));
        // Status's name for the upstream when a local branch is named `origin/fix` too.
        assert_eq!(remote_branch(Some("remotes/origin/fix"), &remotes, "origin/fix"), pair("origin", "fix"));
        assert_eq!(remote_branch(Some("remotes/team/x/fix"), &remotes, "local"), pair("team/x", "fix"));
        assert_eq!(remote_branch(Some("remotes/x"), &["remotes".to_string()], "local"), pair("remotes", "x"));
        assert_eq!(remote_branch(None, &["fork".to_string()], "wip"), pair("fork", "wip"));
        assert_eq!(remote_branch(None, &[], "wip"), None);
        assert_eq!(remote_branch(Some("origin/wip"), &[], "wip"), None);
    }

    /// The `$name`s in GraphQL text.
    fn variables(text: &str) -> BTreeSet<&str> {
        text.split('$')
            .skip(1)
            .map(|rest| &rest[..rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len())])
            .collect()
    }

    #[test]
    fn query_uses_every_variable_it_declares_and_interpolates_nothing() {
        let targets: Vec<PrTarget> = (0..12)
            .map(|i| target(&format!("acme{i}"), &format!("widget-{i}"), &format!("feat/\"{i}\" {{x}}"), None))
            .collect();
        let body = build_query(&targets);
        let query = body["query"].as_str().unwrap();
        let (header, selection) = query.split_once('{').unwrap();
        let declared = variables(header);
        assert_eq!(declared.len(), 4 * targets.len());
        assert_eq!(variables(selection), declared);
        let values = body["variables"].as_object().unwrap();
        assert_eq!(values.keys().map(String::as_str).collect::<BTreeSet<_>>(), declared);
        for (i, t) in targets.iter().enumerate() {
            assert!(query.contains(&format!("r{i}: repository(owner: $o{i}, name: $n{i})")), "r{i}");
            assert_eq!(values[&format!("o{i}")], t.repo.owner.as_str());
            assert_eq!(values[&format!("n{i}")], t.repo.name.as_str());
            assert_eq!(values[&format!("b{i}")], t.branch.as_str());
            assert_eq!(values[&format!("q{i}")], format!("refs/heads/{}", t.branch).as_str());
        }
        // The text only depends on how many targets there are.
        let others: Vec<PrTarget> = (0..12).map(|i| target("x", "y", &format!("b{i}"), None)).collect();
        assert_eq!(build_query(&others)["query"], body["query"]);
        assert!(!query.contains("acme") && !query.contains("widget") && !query.contains("feat/"));
    }

    /// A real answer from `gh api graphql` (gh 2.98, exit code 1): one repo found, one not.
    const PARTIAL_FAILURE: &str = r#"{"data":{"r0":{"nameWithOwner":"hevi-public/ubergit","defaultBranchRef":{"name":"main"},"pullRequests":{"nodes":[{"number":15,"title":"Confirm before quitting on q","url":"https://github.com/hevi-public/ubergit/pull/15","state":"MERGED","isDraft":false,"updatedAt":"2026-09-26T15:58:56Z","reviewDecision":null,"headRefOid":"99ab5fc9011cd5d61583a1aed388322fec58f255","baseRefName":"main","headRepository":{"nameWithOwner":"hevi-public/ubergit"},"baseRepository":{"nameWithOwner":"hevi-public/ubergit"},"reviewRequests":{"nodes":[]},"latestOpinionatedReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}]},"ref":{"associatedPullRequests":{"nodes":[{"number":15,"title":"Confirm before quitting on q","url":"https://github.com/hevi-public/ubergit/pull/15","state":"MERGED","isDraft":false,"updatedAt":"2026-09-26T15:58:56Z","reviewDecision":null,"headRefOid":"99ab5fc9011cd5d61583a1aed388322fec58f255","baseRefName":"main","headRepository":{"nameWithOwner":"hevi-public/ubergit"},"baseRepository":{"nameWithOwner":"hevi-public/ubergit"},"reviewRequests":{"nodes":[]},"latestOpinionatedReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}]}},"parent":null},"r1":null},"errors":[{"type":"NOT_FOUND","path":["r1"],"locations":[{"line":3,"column":3}],"message":"Could not resolve to a Repository with the name 'hevi-public/does-not-exist-xyz'."}]}"#;

    const MERGED_TIP: &str = "99ab5fc9011cd5d61583a1aed388322fec58f255";

    fn partial_failure_targets(local_oid: &str) -> [PrTarget; 2] {
        [
            target("hevi-public", "ubergit", "confirm-quit", Some(local_oid)),
            target("hevi-public", "does-not-exist-xyz", "main", None),
        ]
    }

    #[test]
    fn reads_an_answer_where_one_repo_failed() {
        let results = parse_response(PARTIAL_FAILURE.as_bytes(), &partial_failure_targets(MERGED_TIP)).unwrap();
        let merged = PullRequest {
            number: 15,
            title: "Confirm before quitting on q".into(),
            url: "https://github.com/hevi-public/ubergit/pull/15".into(),
            state: PrState::Merged,
            review: None,
            checks: None,
            reviewers: vec![],
            base_ref: "main".into(),
            base_repo: "hevi-public/ubergit".into(),
            head_oid: MERGED_TIP.into(),
        };
        assert_eq!(
            results,
            vec![
                Ok(PrLookup {
                    pr: Some(merged),
                    create_url: None
                }),
                Err("Could not resolve to a Repository with the name 'hevi-public/does-not-exist-xyz'.".into()),
            ]
        );

        // New commits on the branch since: that merged PR isn't for them.
        let results = parse_response(PARTIAL_FAILURE.as_bytes(), &partial_failure_targets("0123abc")).unwrap();
        assert_eq!(
            results[0],
            Ok(PrLookup {
                pr: None,
                create_url: Some(
                    "https://github.com/hevi-public/ubergit/compare/main...hevi-public:confirm-quit?expand=1".into()
                ),
            })
        );
    }

    #[test]
    fn an_answer_without_data_is_an_error() {
        let targets = [target("o", "r", "b", None)];
        assert!(parse_response(b"<html>502 Bad Gateway</html>", &targets).is_err());
        assert!(parse_response(b"{}", &targets).is_err());
        assert!(parse_response(br#"{"data":null}"#, &targets).is_err());
        let unused = r#"{"errors":[{"message":"Variable $b1 is declared by anonymous query but not used"}]}"#;
        assert_eq!(
            parse_response(unused.as_bytes(), &targets),
            Err("Variable $b1 is declared by anonymous query but not used".into())
        );

        // A repo missing without an error for it, or of an unexpected shape, fails alone.
        let results = parse_response(br#"{"data":{"r0":{"nameWithOwner":7}}}"#, &targets).unwrap();
        assert!(results[0].is_err());
        let results = parse_response(br#"{"data":{}}"#, &targets).unwrap();
        assert_eq!(results, vec![Err("no answer from GitHub".into())]);
    }

    #[test]
    fn an_error_inside_a_repos_answer_fails_that_repo() {
        let targets = [target("o", "r", "a", None), target("o", "r", "b", None)];
        // GitHub nulls the field that failed and keeps the repo: that isn't "no PR".
        let mut failed = repo_node("o/r", vec![], Value::Null);
        failed["pullRequests"] = Value::Null;
        let body = json!({
            "data": { "r0": repo_node("o/r", vec![], Value::Null), "r1": failed },
            "errors": [{ "path": ["r1", "pullRequests"], "message": "Something went wrong while executing your query." }],
        });
        let results = parse_response(body.to_string().as_bytes(), &targets).unwrap();
        assert!(results[0].is_ok());
        assert_eq!(results[1], Err("Something went wrong while executing your query.".into()));

        let mut failed = repo_node("o/r", vec![], Value::Null);
        failed["ref"] = json!({ "associatedPullRequests": null });
        let body = json!({
            "data": { "r0": failed, "r1": repo_node("o/r", vec![], Value::Null) },
            "errors": [{ "path": ["r0", "ref", "associatedPullRequests"], "message": "timedout" }],
        });
        let results = parse_response(body.to_string().as_bytes(), &targets).unwrap();
        assert_eq!(results[0], Err("timedout".into()));
        assert!(results[1].is_ok());
    }

    /// A PR from `head` (`owner/name`) into `o/r`, as the query's `Pr` fragment returns it,
    /// without reviews or checks.
    fn pr(number: u64, state: &str, head: Option<&str>, head_oid: &str, updated_at: &str) -> Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "url": format!("https://github.com/o/r/pull/{number}"),
            "state": state,
            "isDraft": false,
            "updatedAt": updated_at,
            "reviewDecision": null,
            "headRefOid": head_oid,
            "baseRefName": "main",
            "headRepository": head.map(|name| json!({ "nameWithOwner": name })),
            "baseRepository": { "nameWithOwner": "o/r" },
            "reviewRequests": { "nodes": [] },
            "latestOpinionatedReviews": { "nodes": [] },
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": null } }] },
        })
    }

    /// A repo whose PRs from a branch of the target's name are `prs`. The branch itself
    /// isn't on it.
    fn repo_node(name_with_owner: &str, prs: Vec<Value>, parent: Value) -> Value {
        json!({
            "nameWithOwner": name_with_owner,
            "defaultBranchRef": { "name": "main" },
            "pullRequests": { "nodes": prs },
            "ref": null,
            "parent": parent,
        })
    }

    fn resolve_one(repo: Value, target: &PrTarget) -> PrLookup {
        let body = json!({ "data": { "r0": repo } }).to_string();
        parse_response(body.as_bytes(), std::slice::from_ref(target))
            .unwrap()
            .remove(0)
            .unwrap()
    }

    /// The number of the PR picked for branch `feature` of `o/r`.
    fn picked(prs: Vec<Value>, local_oid: Option<&str>) -> Option<u64> {
        let found = resolve_one(repo_node("o/r", prs, Value::Null), &target("o", "r", "feature", local_oid));
        found.pr.map(|pr| pr.number)
    }

    #[test]
    fn picks_the_newest_open_pr_else_a_merged_one_at_the_local_tip() {
        let merged = pr(1, "MERGED", Some("o/r"), "aaa", "2026-01-03T00:00:00Z");
        let open = pr(2, "OPEN", Some("o/r"), "bbb", "2026-01-01T00:00:00Z");
        let newer_open = pr(3, "OPEN", Some("o/r"), "ccc", "2026-01-02T00:00:00Z");
        let closed = pr(4, "CLOSED", Some("o/r"), "ddd", "2026-01-04T00:00:00Z");
        assert_eq!(picked(vec![merged.clone(), open.clone(), newer_open], Some("aaa")), Some(3));
        assert_eq!(picked(vec![merged.clone(), open], Some("aaa")), Some(2));
        assert_eq!(picked(vec![merged.clone()], Some("aaa")), Some(1));
        // The branch name was used again after that PR merged.
        assert_eq!(picked(vec![merged.clone()], Some("fff")), None);
        assert_eq!(picked(vec![merged.clone()], None), None);
        assert_eq!(picked(vec![closed.clone()], Some("ddd")), None);
        assert_eq!(picked(vec![closed, merged], Some("aaa")), Some(1));
    }

    #[test]
    fn only_picks_prs_whose_head_is_this_repo() {
        let another_fork = pr(1, "OPEN", Some("someone-else/r"), "a", "2026-01-02T00:00:00Z");
        let fork_deleted = pr(2, "OPEN", None, "b", "2026-01-03T00:00:00Z");
        // The same owner, but another repo with a branch of that name.
        let other_repo = pr(4, "OPEN", Some("o/other"), "d", "2026-01-04T00:00:00Z");
        let ours = pr(3, "OPEN", Some("O/R"), "c", "2026-01-01T00:00:00Z");
        let theirs = vec![another_fork, fork_deleted, other_repo];
        assert_eq!(picked([theirs.clone(), vec![ours]].concat(), None), Some(3));
        assert_eq!(picked(theirs, None), None);
    }

    #[test]
    fn follows_a_renamed_owner() {
        // The remote's URL still names the old owner; GitHub answers with the new one.
        let ours = pr(3, "OPEN", Some("new-owner/r"), "c", "2026-01-01T00:00:00Z");
        let repo = repo_node("new-owner/r", vec![ours], Value::Null);
        let found = resolve_one(repo, &target("old-owner", "r", "feature", None));
        assert_eq!(found.pr.map(|pr| pr.number), Some(3));
    }

    #[test]
    fn finds_a_forks_pr_on_the_parent_through_its_branch() {
        let mut draft = pr(7, "OPEN", Some("me/tool"), "abc", "2026-02-01T00:00:00Z");
        draft["isDraft"] = json!(true);
        draft["baseRefName"] = json!("develop");
        draft["baseRepository"] = json!({ "nameWithOwner": "upstream/tool" });
        let mut old_in_fork = pr(3, "OPEN", Some("me/tool"), "y", "2026-01-01T00:00:00Z");
        old_in_fork["baseRepository"] = json!({ "nameWithOwner": "me/tool" });
        let parent = repo_node("upstream/tool", vec![], Value::Null);
        let mut fork = repo_node("me/tool", vec![old_in_fork.clone()], parent);
        // The branch's PRs, into the parent and into the fork itself.
        fork["ref"] = json!({ "associatedPullRequests": { "nodes": [draft, old_in_fork] } });

        let found = resolve_one(fork, &target("me", "tool", "feature", None));
        let pr = found.pr.unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.state, PrState::Draft);
        assert_eq!(pr.base_repo, "upstream/tool");
        assert_eq!(pr.base_ref, "develop");
        assert_eq!(found.create_url, None);
    }

    #[test]
    fn create_url_proposes_a_forks_branch_to_the_parent() {
        let mut parent = repo_node("upstream/tool", vec![], Value::Null);
        parent["defaultBranchRef"]["name"] = json!("develop");
        let fork = repo_node("Me/tool", vec![], parent);
        let url = |branch| resolve_one(fork.clone(), &target("me", "tool", branch, None)).create_url;
        assert_eq!(
            url("fix/ü #1").as_deref(),
            Some("https://github.com/upstream/tool/compare/develop...Me:fix/%C3%BC%20%231?expand=1")
        );
        // Even the fork's own default branch.
        assert_eq!(
            url("main").as_deref(),
            Some("https://github.com/upstream/tool/compare/develop...Me:main?expand=1")
        );
    }

    #[test]
    fn create_url_proposes_a_branch_to_its_own_repo() {
        let own = repo_node("o/r", vec![], Value::Null);
        let url = |repo: &Value, target: PrTarget| resolve_one(repo.clone(), &target).create_url;
        assert_eq!(
            url(&own, target("o", "r", "feat/a+b", None)).as_deref(),
            Some("https://github.com/o/r/compare/main...o:feat/a%2Bb?expand=1")
        );
        let ghe = PrTarget {
            repo: repo("ghe.example.com", "o", "r"),
            ..target("o", "r", "x", None)
        };
        assert_eq!(
            url(&own, ghe).as_deref(),
            Some("https://ghe.example.com/o/r/compare/main...o:x?expand=1")
        );
        // Nothing to propose from the default branch, or to an empty repo.
        assert_eq!(url(&own, target("o", "r", "main", None)), None);
        let mut empty = own.clone();
        empty["defaultBranchRef"] = Value::Null;
        assert_eq!(url(&empty, target("o", "r", "x", None)), None);
    }

    fn checks_of(rollup: Value) -> Option<Checks> {
        let mut open = pr(1, "OPEN", Some("o/r"), "a", "2026-01-01T00:00:00Z");
        open["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = rollup;
        let found = resolve_one(repo_node("o/r", vec![open], Value::Null), &target("o", "r", "feature", None));
        found.pr.unwrap().checks
    }

    fn check_run(name: &str, status: &str, conclusion: Option<&str>) -> Value {
        json!({ "__typename": "CheckRun", "name": name, "status": status, "conclusion": conclusion })
    }

    fn status_context(context: &str, state: &str) -> Value {
        json!({ "__typename": "StatusContext", "context": context, "state": state })
    }

    fn rollup(state: &str, total: u32, contexts: Vec<Value>) -> Value {
        json!({ "state": state, "contexts": { "totalCount": total, "nodes": contexts } })
    }

    #[test]
    fn maps_check_rollups() {
        assert_eq!(checks_of(Value::Null), None);

        let mixed = rollup(
            "FAILURE",
            60,
            vec![
                check_run("build", "COMPLETED", Some("SUCCESS")),
                check_run("test", "COMPLETED", Some("FAILURE")),
                check_run("lint", "COMPLETED", Some("TIMED_OUT")),
                check_run("test", "COMPLETED", Some("CANCELLED")),
                check_run("docs", "COMPLETED", Some("SKIPPED")),
                check_run("e2e", "IN_PROGRESS", None),
                status_context("ci/jenkins", "ERROR"),
                status_context("coverage", "PENDING"),
                status_context("deploy", "SUCCESS"),
                Value::Null,
            ],
        );
        assert_eq!(
            checks_of(mixed),
            Some(Checks {
                state: ChecksState::Failing,
                failing: vec!["test".into(), "lint".into(), "ci/jenkins".into()],
                total: 60,
                pending: 2,
            })
        );

        let waiting = rollup(
            "PENDING",
            3,
            vec![
                check_run("build", "QUEUED", None),
                check_run("deploy", "WAITING", None),
                status_context("review", "EXPECTED"),
            ],
        );
        assert_eq!(
            checks_of(waiting),
            Some(Checks {
                state: ChecksState::Pending,
                failing: vec![],
                total: 3,
                pending: 3,
            })
        );

        let passing = rollup("SUCCESS", 1, vec![check_run("build", "COMPLETED", Some("SUCCESS"))]);
        assert_eq!(checks_of(passing).map(|c| (c.state, c.pending)), Some((ChecksState::Passing, 0)));
        let state = |state| checks_of(rollup(state, 0, vec![])).map(|c| c.state);
        assert_eq!(state("ERROR"), Some(ChecksState::Failing));
        assert_eq!(state("EXPECTED"), Some(ChecksState::Pending));
    }

    #[test]
    fn merges_requested_and_submitted_reviews() {
        let mut open = pr(1, "OPEN", Some("o/r"), "a", "2026-01-01T00:00:00Z");
        open["reviewDecision"] = json!("CHANGES_REQUESTED");
        open["reviewRequests"] = json!({ "nodes": [
            { "requestedReviewer": { "login": "alice" } },
            { "requestedReviewer": { "slug": "core-team" } },
            { "requestedReviewer": null },
            null,
        ]});
        open["latestOpinionatedReviews"] = json!({ "nodes": [
            { "author": { "login": "bob" }, "state": "APPROVED" },
            // Asked again after asking for changes: waiting on her again.
            { "author": { "login": "Alice" }, "state": "CHANGES_REQUESTED" },
            { "author": { "login": "carol" }, "state": "CHANGES_REQUESTED" },
            { "author": { "login": "dave" }, "state": "COMMENTED" },
            { "author": null, "state": "APPROVED" },
        ]});
        let found = resolve_one(repo_node("o/r", vec![open], Value::Null), &target("o", "r", "feature", None));
        let pr = found.pr.unwrap();
        assert_eq!(pr.review, Some(ReviewDecision::ChangesRequested));
        let reviewer = |name: &str, state| Reviewer {
            name: name.into(),
            state,
        };
        assert_eq!(
            pr.reviewers,
            vec![
                reviewer("alice", ReviewerState::Requested),
                reviewer("core-team", ReviewerState::Requested),
                reviewer("bob", ReviewerState::Approved),
                reviewer("carol", ReviewerState::ChangesRequested),
            ]
        );
    }

    #[test]
    fn reads_the_hosts_with_a_working_login() {
        let status = br#"{"hosts":{
            "github.com":[{"active":true,"gitProtocol":"https","host":"github.com","login":"me","scopes":"repo","state":"success","tokenSource":"keyring"}],
            "ghe.broken.example":[{"active":true,"host":"ghe.broken.example","login":"me","state":"error","tokenSource":"keyring"}],
            "ghe.switched.example":[
                {"active":false,"host":"ghe.switched.example","login":"old","state":"error"},
                {"active":true,"host":"ghe.switched.example","login":"me","state":"success"}],
            "ghe.inactive.example":[
                {"active":true,"host":"ghe.inactive.example","login":"a","state":"timeout"},
                {"active":false,"host":"ghe.inactive.example","login":"b","state":"success"}]
        }}"#;
        assert_eq!(
            parse_auth_hosts(status),
            Some(vec!["ghe.switched.example".to_string(), "github.com".to_string()])
        );
        assert_eq!(parse_auth_hosts(br#"{"hosts":{}}"#), Some(vec![]));
        assert_eq!(parse_auth_hosts(b"not json"), None);
    }

    /// A stand-in `gh` that runs `script`.
    fn fake_gh(dir: &Path, script: &str) -> Gh {
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh::new(Arc::new(())).with_program(path)
    }

    #[test]
    fn an_older_gh_without_json_auth_status_means_github_com_only() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo 'unknown flag: --json' >&2; exit 1");
        assert_eq!(block_on(logged_in_hosts(&gh, dir.path())).unwrap(), vec!["github.com".to_string()]);
    }

    /// A stand-in for `gh api graphql` that logs its arguments, working directory, input
    /// and how many repos it was asked about, and answers that each one exists without PRs.
    const ANSWER_EVERY_REPO: &str = r#"
dir=$(dirname "$0")
body=$(cat)
echo "$*" >> "$dir/args"
pwd -P >> "$dir/cwd"
printf '%s\n' "$body" >> "$dir/bodies"
n=$(( $(printf '%s' "$body" | grep -o '"n[0-9]*":' | wc -l) ))
echo "$n" >> "$dir/calls"
printf '{"data":{'
i=0
while [ "$i" -lt "$n" ]; do
  [ "$i" -gt 0 ] && printf ','
  printf '"r%d":{"nameWithOwner":"o/r","defaultBranchRef":{"name":"main"},"pullRequests":{"nodes":[]},"parent":null}' "$i"
  i=$((i + 1))
done
printf '}}'
"#;

    fn read(dir: &Path, file: &str) -> String {
        std::fs::read_to_string(dir.join(file)).unwrap()
    }

    #[test]
    fn lookup_sends_the_query_to_the_host_once_per_repo_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), ANSWER_EVERY_REPO);
        let on_ghe = |branch: &str, local_oid: Option<&str>| PrTarget {
            repo: repo("ghe.example.com", "o", "r"),
            ..target("o", "r", branch, local_oid)
        };
        // The third is a second clone on the same branch.
        let targets = [on_ghe("a", None), on_ghe("b", None), on_ghe("a", Some("abc"))];
        let workdir = tempfile::tempdir().unwrap();
        let results = block_on(lookup(&gh, "ghe.example.com", &targets, workdir.path())).unwrap();

        assert_eq!(read(dir.path(), "args"), "api graphql --hostname ghe.example.com --input -\n");
        let cwd = workdir.path().canonicalize().unwrap();
        assert_eq!(read(dir.path(), "cwd"), format!("{}\n", cwd.display()));
        let body: Value = serde_json::from_str(&read(dir.path(), "bodies")).unwrap();
        assert_eq!(body, build_query(&targets[..2]));
        let create = |branch: &str| {
            Ok(PrLookup {
                pr: None,
                create_url: Some(format!("https://ghe.example.com/o/r/compare/main...o:{branch}?expand=1")),
            })
        };
        assert_eq!(results, vec![create("a"), create("b"), create("a")]);
    }

    #[test]
    fn lookup_asks_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), ANSWER_EVERY_REPO);
        let targets: Vec<PrTarget> = (0..85).map(|i| target("o", "r", &format!("b{i}"), None)).collect();
        let results = block_on(lookup(&gh, "github.com", &targets, dir.path())).unwrap();

        assert_eq!(read(dir.path(), "calls"), "40\n40\n5\n");
        assert_eq!(results.len(), 85);
        for (i, result) in results.iter().enumerate() {
            let url = result.as_ref().unwrap().create_url.as_deref().unwrap();
            assert!(url.ends_with(&format!("...o:b{i}?expand=1")), "{i}: {url}");
        }
    }

    #[test]
    fn lookup_keeps_the_answer_when_one_repo_failed() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "cat >/dev/null\ncat <<'EOF'\n{PARTIAL_FAILURE}\nEOF\n\
             echo \"gh: Could not resolve to a Repository with the name 'hevi-public/does-not-exist-xyz'.\" >&2\nexit 1"
        );
        let gh = fake_gh(dir.path(), &script);
        let results = block_on(lookup(&gh, "github.com", &partial_failure_targets(MERGED_TIP), dir.path())).unwrap();
        assert_eq!(results[0].as_ref().unwrap().pr.as_ref().map(|pr| pr.number), Some(15));
        assert!(results[1].as_ref().unwrap_err().starts_with("Could not resolve"));
    }

    #[test]
    fn lookup_fails_when_gh_does() {
        let dir = tempfile::tempdir().unwrap();
        let targets = [target("o", "r", "b", None)];

        let gh = fake_gh(dir.path(), "cat >/dev/null; echo 'gh auth login' >&2; exit 4");
        let result = block_on(lookup(&gh, "github.com", &targets, dir.path()));
        assert!(matches!(result, Err(GhError::NotLoggedIn { .. })), "{result:?}");
        // Nothing to ask, so gh doesn't run.
        assert_eq!(block_on(lookup(&gh, "github.com", &[], dir.path())).unwrap(), vec![]);

        // Any other failure is the repos'.
        let gh = fake_gh(
            dir.path(),
            r#"cat >/dev/null; echo '{"errors":[{"message":"Something went wrong"}]}'; echo 'gh: Something went wrong' >&2; exit 1"#,
        );
        let results = block_on(lookup(&gh, "github.com", &targets, dir.path())).unwrap();
        assert_eq!(results, vec![Err("gh: Something went wrong".into())]);

        // gh succeeded, but that isn't an answer.
        let gh = fake_gh(dir.path(), "cat >/dev/null; echo 'not json'");
        let results = block_on(lookup(&gh, "github.com", &targets, dir.path())).unwrap();
        assert!(results[0].as_ref().unwrap_err().starts_with("unexpected answer from GitHub"));
    }

    #[test]
    fn a_failed_call_fails_only_its_own_repos() {
        let dir = tempfile::tempdir().unwrap();
        // The second call fails.
        let script = format!(
            "if [ -e \"$(dirname \"$0\")/answered\" ]; then\n  cat >/dev/null; echo 'gh: HTTP 502: Bad Gateway' >&2; exit 1\nfi\n\
             touch \"$(dirname \"$0\")/answered\"\n{ANSWER_EVERY_REPO}"
        );
        let gh = fake_gh(dir.path(), &script);
        let targets: Vec<PrTarget> = (0..45).map(|i| target("o", "r", &format!("b{i}"), None)).collect();
        let results = block_on(lookup(&gh, "github.com", &targets, dir.path())).unwrap();

        assert_eq!(read(dir.path(), "calls"), "40\n");
        assert!(results[..40].iter().all(|result| result.as_ref().is_ok_and(|found| found.pr.is_none())));
        assert_eq!(results[40..], vec![Err("gh: HTTP 502: Bad Gateway".to_string()); 5]);
    }
}
