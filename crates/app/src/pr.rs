//! How a repo's pull request reads: the overview's PR column, the badge after the branch in
//! the Repos panel, and the Status view's rows. All of it goes by [`RepoStore::pr_view`],
//! so another branch's PR never shows.
//!
//! [`RepoStore::pr_view`]: crate::store::RepoStore::pr_view

use gpui_kit::Hsla;
use ubergit_core::github::RemoteRepo;
use ubergit_core::pr_status::{self, GhStatus, LookupError, PrView, Skip};
use ubergit_core::{Checks, ChecksState, PrState, PullRequest, ReviewDecision, Reviewer, ReviewerState};

use crate::store::RepoEntry;
use crate::text::{Line, age, truncate};
use crate::theme::Palette;

/// Whether the overview has a PR column. Not while gh can't be asked: every cell would be
/// blank, and the Status view says why.
pub fn column_shown(gh: &GhStatus) -> bool {
    matches!(gh, GhStatus::Unchecked | GhStatus::Ready { .. })
}

/// The word for where a PR stands, and its colour. A draft isn't up for review yet, so it's
/// a draft whatever the review decision says.
fn state_word(pr: &PullRequest) -> (&'static str, Hsla) {
    match (pr.state, pr.review) {
        (PrState::Merged, _) => ("merged", Palette::magenta()),
        (PrState::Draft, _) => ("draft", Palette::dim()),
        (PrState::Open, Some(ReviewDecision::Approved)) => ("approved", Palette::green()),
        (PrState::Open, Some(ReviewDecision::ChangesRequested)) => ("changes", Palette::red()),
        (PrState::Open, Some(ReviewDecision::ReviewRequired) | None) => ("open", Palette::cyan()),
    }
}

/// `✓` passing, `✗` failing, `●` still running. Nothing without checks, or once merged,
/// when they no longer matter.
fn checks_glyph(pr: &PullRequest) -> Option<(&'static str, Hsla)> {
    if pr.state == PrState::Merged {
        return None;
    }
    Some(match pr.checks.as_ref()?.state {
        ChecksState::Passing => ("✓", Palette::green()),
        ChecksState::Failing => ("✗", Palette::red()),
        ChecksState::Pending => ("●", Palette::yellow()),
    })
}

/// The overview's cell: `#412 approved ✓`, `-` for no PR, `…` until it's known, `?` when
/// looking it up failed, blank for a branch that can't have one. A failed lookup after an
/// answer keeps showing the answer; the Status view has the error.
pub fn cell(view: &PrView) -> Line {
    let mut line = Line::new();
    match view {
        PrView::Found { found, .. } => match &found.lookup.pr {
            Some(pr) => {
                let (word, color) = state_word(pr);
                line.color(format!("#{} {word}", pr.number), color);
                if let Some((glyph, color)) = checks_glyph(pr) {
                    line.push(" ");
                    line.color(glyph, color);
                }
            }
            None => {
                line.color("-", Palette::dim());
            }
        },
        PrView::Unknown => {
            line.color("…", Palette::dim());
        }
        PrView::Failed(_) => {
            line.color("?", Palette::red().opacity(0.6));
        }
        PrView::Off | PrView::Skipped(_) | PrView::GhNotInstalled | PrView::GhNotLoggedIn | PrView::GhFailed(_) => {}
    }
    line
}

/// ` #412✓` after the branch in the Repos panel, coloured like the overview's word. Only for
/// a PR: the panel is too narrow for the rest.
pub fn badge(view: &PrView) -> Option<Line> {
    let PrView::Found { found, .. } = view else { return None };
    let pr = found.lookup.pr.as_ref()?;
    let mut line = Line::new();
    line.color(format!(" #{}", pr.number), state_word(pr).1);
    if let Some((glyph, color)) = checks_glyph(pr) {
        line.color(glyph, color);
    }
    Some(line)
}

/// The GitHub repo of the remote the checked-out branch's PR would come from, once its URL
/// has been read: to tell a fork's PR apart, and to name the host that isn't GitHub.
pub fn remote_repo(entry: &RepoEntry) -> Option<&RemoteRepo> {
    let key = pr_status::pr_key(entry.summary.as_ref()?, entry.location.bare).ok()?;
    entry.pr.remotes.get(&key.remote)?.as_ref()
}

/// The Status view's rows as (key, value): the PR, its reviews and checks, and when it was
/// looked up; or one row saying why there's none. Nothing while PR status is off. `width`
/// is the characters a value has; titles and lists are cut to fit it.
pub fn status_rows(view: &PrView, remote: Option<&RemoteRepo>, width: usize) -> Vec<(&'static str, Line)> {
    let mut rows = Vec::new();
    let reason = match view {
        PrView::Off => return rows,
        PrView::Found { found, error } => {
            let Some(pr) = &found.lookup.pr else {
                let mut none = Line::new();
                none.color(format!("none  checked {} ago", age(Some(found.checked))), Palette::dim());
                if let Some(error) = error {
                    failure(&mut none, error, width);
                }
                rows.push(("Pull request", none));
                return rows;
            };
            rows.push(("Pull request", title_line(pr, remote, width)));
            if !pr.reviewers.is_empty() {
                rows.push(("Reviews", reviews(&pr.reviewers, width)));
            }
            if let Some(checks) = &pr.checks {
                rows.push(("Checks", checks_line(checks, width)));
            }
            let mut checked = Line::new();
            checked.color(format!("{} ago", age(Some(found.checked))), Palette::dim());
            if let Some(error) = error {
                failure(&mut checked, error, width);
            }
            rows.push(("Checked", checked));
            return rows;
        }
        PrView::Failed(error) => {
            let mut line = Line::new();
            failure(&mut line, error, width);
            rows.push(("Pull request", line));
            return rows;
        }
        PrView::Unknown => "looking up…".to_string(),
        PrView::Skipped(skip) => skip_reason(*skip, remote),
        PrView::GhNotInstalled => "gh isn't installed (brew install gh)".to_string(),
        PrView::GhNotLoggedIn => "gh isn't logged in: run gh auth login".to_string(),
        PrView::GhFailed(message) => message.lines().next().unwrap_or_default().to_string(),
    };
    let mut line = Line::new();
    line.color(truncate(&reason, width), Palette::dim());
    rows.push(("Pull request", line));
    rows
}

fn skip_reason(skip: Skip, remote: Option<&RemoteRepo>) -> String {
    match skip {
        Skip::Bare => "bare repository".into(),
        Skip::Detached => "detached HEAD".into(),
        Skip::Unborn => "no commits yet".into(),
        Skip::DefaultBranch => "on the default branch".into(),
        Skip::NoRemote => "no remote".into(),
        // A GitHub Enterprise host gh has no login for looks the same as GitLab.
        Skip::NotOnGitHub => match remote {
            Some(repo) => format!("not on GitHub ({} isn't a host gh is logged in to)", repo.host),
            None => "not on GitHub".into(),
        },
    }
}

/// `#412 Add tax rounding  approved  → main`, with the title cut so the state and base fit.
fn title_line(pr: &PullRequest, remote: Option<&RemoteRepo>, width: usize) -> Line {
    let (word, color) = state_word(pr);
    let number = format!("#{} ", pr.number);
    let word = format!("  {word}");
    let base = base_label(pr, remote);
    let fixed = number.chars().count() + word.chars().count() + "  → ".chars().count() + base.chars().count();
    let mut line = Line::new();
    line.color(number, color);
    line.push(truncate(&pr.title, width.saturating_sub(fixed).max(20)));
    line.color(word, color);
    line.color("  → ", Palette::dim());
    line.color(base, Palette::blue());
    line
}

/// The branch a PR merges into, with its repo when that isn't the remote's, as for a fork's
/// PR on its parent.
fn base_label(pr: &PullRequest, remote: Option<&RemoteRepo>) -> String {
    let elsewhere = remote.is_some_and(|repo| !pr.base_repo.eq_ignore_ascii_case(&format!("{}/{}", repo.owner, repo.name)));
    if elsewhere {
        format!("{}:{}", pr.base_repo, pr.base_ref)
    } else {
        pr.base_ref.clone()
    }
}

/// `alice ✓  bob ✗  platform-team (requested)`, as many as fit in `width`. Those who have
/// reviewed come first: when some must be left out, it's better they're the ones asked.
fn reviews(reviewers: &[Reviewer], width: usize) -> Line {
    let mark = |state: ReviewerState| match state {
        ReviewerState::Approved => (" ✓", Palette::green()),
        ReviewerState::ChangesRequested => (" ✗", Palette::red()),
        ReviewerState::Requested => (" (requested)", Palette::dim()),
    };
    let mut reviewers: Vec<&Reviewer> = reviewers.iter().collect();
    reviewers.sort_by_key(|r| r.state == ReviewerState::Requested);
    let widths: Vec<usize> = reviewers
        .iter()
        .map(|r| r.name.chars().count() + mark(r.state).0.chars().count())
        .collect();
    let shown = fitting(&widths, 2, width);
    let mut line = Line::new();
    for (ix, reviewer) in reviewers[..shown].iter().enumerate() {
        if ix > 0 {
            line.push("  ");
        }
        line.push(&reviewer.name);
        let (text, color) = mark(reviewer.state);
        line.color(text, color);
    }
    if shown < reviewers.len() {
        line.color(format!(" and {} more", reviewers.len() - shown), Palette::dim());
    }
    line
}

/// `2 failing: build, e2e`, `pending: 3 of 14` or `passing (14)`. Failing checks with
/// others still running say so, since more may fail.
fn checks_line(checks: &Checks, width: usize) -> Line {
    let mut line = Line::new();
    match checks.state {
        ChecksState::Failing => {
            let running = (checks.pending > 0).then(|| format!(", {} still running", checks.pending));
            if checks.failing.is_empty() {
                line.color("failing", Palette::red());
            } else {
                let head = format!("{} failing: ", checks.failing.len());
                let room = width.saturating_sub(head.len() + running.as_ref().map_or(0, String::len));
                line.color(format!("{head}{}", names(&checks.failing, room)), Palette::red());
            }
            if let Some(running) = running {
                line.color(running, Palette::yellow());
            }
        }
        ChecksState::Pending if checks.pending > 0 => {
            line.color(format!("pending: {} of {}", checks.pending, checks.total), Palette::yellow());
        }
        ChecksState::Pending => {
            line.color("pending", Palette::yellow());
        }
        ChecksState::Passing => {
            line.color(format!("passing ({})", checks.total), Palette::green());
        }
    }
    line
}

/// `build, e2e and 3 more`: as many names as fit in `width`.
fn names(names: &[String], width: usize) -> String {
    let widths: Vec<usize> = names.iter().map(|n| n.chars().count()).collect();
    let shown = fitting(&widths, 2, width);
    let mut out = names[..shown].iter().map(|n| truncate(n, width.max(10))).collect::<Vec<_>>().join(", ");
    if shown < names.len() {
        out.push_str(&format!(" and {} more", names.len() - shown));
    }
    out
}

/// How many items of these widths, `sep` apart, fit in `width`: all of them, or as many as
/// leave room for ` and N more`, but always one.
fn fitting(widths: &[usize], sep: usize, width: usize) -> usize {
    let total = widths.iter().sum::<usize>() + sep * widths.len().saturating_sub(1);
    if total <= width {
        return widths.len();
    }
    let room = width.saturating_sub(" and 99 more".len());
    let mut used = 0;
    let mut count = 0;
    for &w in widths {
        let next = if count == 0 { w } else { used + sep + w };
        if count > 0 && next > room {
            break;
        }
        used = next;
        count += 1;
    }
    count
}

/// `failed 1m ago: <message>` in red, after what the line says already.
fn failure(line: &mut Line, error: &LookupError, width: usize) {
    let gap = if line.width() > 0 { "  " } else { "" };
    let head = format!("{gap}failed {} ago: ", age(Some(error.at)));
    let room = width.saturating_sub(line.width() + head.chars().count()).max(20);
    let message = error.message.lines().next().unwrap_or_default();
    line.color(format!("{head}{}", truncate(message, room)), Palette::red());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use ubergit_core::github::PrLookup;
    use ubergit_core::pr_status::{Found, PrKey};

    fn key() -> PrKey {
        PrKey {
            remote: "origin".into(),
            branch: "feat/tax".into(),
            remote_oid: Some("abc".into()),
        }
    }

    fn pr(state: PrState, review: Option<ReviewDecision>, checks: Option<ChecksState>) -> PullRequest {
        PullRequest {
            number: 412,
            title: "Add tax rounding".into(),
            url: "https://github.com/acme/billing/pull/412".into(),
            state,
            review,
            checks: checks.map(|state| Checks {
                state,
                failing: if state == ChecksState::Failing { vec!["build".into(), "e2e".into()] } else { vec![] },
                total: 14,
                pending: u32::from(state == ChecksState::Pending) * 3,
            }),
            reviewers: vec![],
            base_ref: "main".into(),
            base_repo: "acme/billing".into(),
            head_oid: "abc".into(),
        }
    }

    fn found(pr: Option<PullRequest>) -> Found {
        Found {
            key: key(),
            lookup: PrLookup { pr, create_url: None },
            checked: SystemTime::now() - Duration::from_secs(120),
            pending_since: None,
        }
    }

    fn error(message: &str) -> LookupError {
        LookupError {
            key: key(),
            message: message.into(),
            at: SystemTime::now() - Duration::from_secs(60),
        }
    }

    fn remote(owner: &str) -> RemoteRepo {
        RemoteRepo {
            host: "github.com".into(),
            owner: owner.into(),
            name: "billing".into(),
        }
    }

    fn spans(line: &Line) -> Vec<(String, Option<Hsla>)> {
        line.spans().into_iter().map(|(text, color)| (text.to_string(), color)).collect()
    }

    fn text(line: &Line) -> String {
        line.spans().into_iter().map(|(text, _)| text).collect()
    }

    fn cell_of(pr: PullRequest) -> Vec<(String, Option<Hsla>)> {
        let found = found(Some(pr));
        spans(&cell(&PrView::Found { found: &found, error: None }))
    }

    fn span(text: &str, color: Hsla) -> (String, Option<Hsla>) {
        (text.to_string(), Some(color))
    }

    #[test]
    fn the_cell_names_the_state_then_the_checks() {
        let cases = [
            (pr(PrState::Open, Some(ReviewDecision::Approved), Some(ChecksState::Passing)), "#412 approved", Palette::green()),
            (pr(PrState::Open, Some(ReviewDecision::ChangesRequested), Some(ChecksState::Passing)), "#412 changes", Palette::red()),
            (pr(PrState::Open, Some(ReviewDecision::ReviewRequired), Some(ChecksState::Passing)), "#412 open", Palette::cyan()),
            (pr(PrState::Open, None, Some(ChecksState::Passing)), "#412 open", Palette::cyan()),
            // A draft is a draft, even approved.
            (pr(PrState::Draft, Some(ReviewDecision::Approved), Some(ChecksState::Passing)), "#412 draft", Palette::dim()),
        ];
        for (pr, word, color) in cases {
            assert_eq!(cell_of(pr), [span(word, color), (" ".into(), None), span("✓", Palette::green())], "{word}");
        }
        let with_checks = |checks| cell_of(pr(PrState::Open, None, checks));
        assert_eq!(with_checks(Some(ChecksState::Failing))[2], span("✗", Palette::red()));
        assert_eq!(with_checks(Some(ChecksState::Pending))[2], span("●", Palette::yellow()));
        assert_eq!(with_checks(None), [span("#412 open", Palette::cyan())]);
        // Checks don't matter once it's merged.
        let merged = pr(PrState::Merged, Some(ReviewDecision::Approved), Some(ChecksState::Failing));
        assert_eq!(cell_of(merged), [span("#412 merged", Palette::magenta())]);
    }

    #[test]
    fn the_cell_says_when_there_is_no_pr_to_show() {
        let none = found(None);
        let failed = error("timed out");
        let cell_text = |view: PrView| text(&cell(&view));
        assert_eq!(cell_text(PrView::Found { found: &none, error: None }), "-");
        // A later failure keeps the answer.
        let open = found(Some(pr(PrState::Open, None, None)));
        assert_eq!(cell_text(PrView::Found { found: &open, error: Some(&failed) }), "#412 open");
        assert_eq!(cell_text(PrView::Unknown), "…");
        assert_eq!(spans(&cell(&PrView::Failed(&failed))), [span("?", Palette::red().opacity(0.6))]);
        assert_eq!(cell_text(PrView::Skipped(Skip::DefaultBranch)), "");
        assert_eq!(cell_text(PrView::Off), "");
    }

    #[test]
    fn the_badge_is_only_for_a_pr() {
        let open = found(Some(pr(PrState::Open, Some(ReviewDecision::Approved), Some(ChecksState::Passing))));
        let badge_of = |view: PrView| badge(&view).map(|line| spans(&line));
        assert_eq!(
            badge_of(PrView::Found { found: &open, error: None }),
            Some(vec![span(" #412", Palette::green()), span("✓", Palette::green())])
        );
        let merged = found(Some(pr(PrState::Merged, None, Some(ChecksState::Passing))));
        assert_eq!(badge_of(PrView::Found { found: &merged, error: None }), Some(vec![span(" #412", Palette::magenta())]));
        let none = found(None);
        assert_eq!(badge_of(PrView::Found { found: &none, error: None }), None);
        assert_eq!(badge_of(PrView::Unknown), None);
    }

    fn rows(view: PrView, remote: Option<&RemoteRepo>, width: usize) -> Vec<(&'static str, String)> {
        status_rows(&view, remote, width).into_iter().map(|(key, line)| (key, text(&line))).collect()
    }

    #[test]
    fn status_rows_describe_the_pr() {
        let mut full = pr(PrState::Open, Some(ReviewDecision::ChangesRequested), Some(ChecksState::Failing));
        full.reviewers = vec![
            Reviewer { name: "alice".into(), state: ReviewerState::Approved },
            Reviewer { name: "bob".into(), state: ReviewerState::ChangesRequested },
            Reviewer { name: "platform-team".into(), state: ReviewerState::Requested },
        ];
        let found = found(Some(full));
        let failed = error("gh: Something went wrong\nmore");
        let view = PrView::Found { found: &found, error: Some(&failed) };
        assert_eq!(
            rows(view, Some(&remote("acme")), 80),
            [
                ("Pull request", "#412 Add tax rounding  changes  → main".into()),
                ("Reviews", "alice ✓  bob ✗  platform-team (requested)".into()),
                ("Checks", "2 failing: build, e2e".into()),
                ("Checked", "2m ago  failed 1m ago: gh: Something went wrong".into()),
            ]
        );
        // A fork's PR is on its parent; a PR without reviewers or checks has no rows for them.
        let quiet = self::found(Some(pr(PrState::Draft, None, None)));
        let view = PrView::Found { found: &quiet, error: None };
        assert_eq!(
            rows(view, Some(&remote("me")), 80),
            [
                ("Pull request", "#412 Add tax rounding  draft  → acme/billing:main".into()),
                ("Checked", "2m ago".into()),
            ]
        );
        // The title gives way, but not below 20 characters.
        let mut long = pr(PrState::Open, None, None);
        long.title = "Round tax per line item instead of per invoice total".into();
        let long = self::found(Some(long));
        let view = PrView::Found { found: &long, error: None };
        assert_eq!(rows(view, None, 40)[0].1, "#412 Round tax per line i…  open  → main");
        assert_eq!(rows(view, None, 10)[0].1, "#412 Round tax per line …  open  → main");
    }

    #[test]
    fn checks_and_reviewers_are_cut_to_fit() {
        let checks = |state, failing: &[&str], pending| Checks {
            state,
            failing: failing.iter().map(|s| s.to_string()).collect(),
            total: 60,
            pending,
        };
        let many = ["build (ubuntu)", "build (macos)", "build (windows)", "e2e", "lint"];
        assert_eq!(text(&checks_line(&checks(ChecksState::Failing, &many, 0), 80)), "5 failing: build (ubuntu), build (macos), build (windows), e2e, lint");
        assert_eq!(text(&checks_line(&checks(ChecksState::Failing, &many, 0), 40)), "5 failing: build (ubuntu) and 4 more");
        assert_eq!(text(&checks_line(&checks(ChecksState::Failing, &many[3..], 2), 60)), "2 failing: e2e, lint, 2 still running");
        assert_eq!(text(&checks_line(&checks(ChecksState::Failing, &[], 0), 60)), "failing");
        assert_eq!(text(&checks_line(&checks(ChecksState::Pending, &[], 3), 60)), "pending: 3 of 60");
        assert_eq!(text(&checks_line(&checks(ChecksState::Pending, &[], 0), 60)), "pending");
        assert_eq!(text(&checks_line(&checks(ChecksState::Passing, &[], 0), 60)), "passing (60)");

        let reviewers: Vec<Reviewer> = ["alice", "bob", "carol", "dave"]
            .iter()
            .map(|name| Reviewer { name: name.to_string(), state: ReviewerState::Requested })
            .collect();
        assert_eq!(text(&reviews(&reviewers, 50)), "alice (requested)  bob (requested) and 2 more");
        assert_eq!(text(&reviews(&reviewers[..1], 5)), "alice (requested)");
        // Reviews given come before reviews asked for.
        let mut mixed = reviewers.clone();
        mixed[2].state = ReviewerState::ChangesRequested;
        mixed[3].state = ReviewerState::Approved;
        assert_eq!(text(&reviews(&mixed, 30)), "carol ✗  dave ✓ and 2 more");
    }

    #[test]
    fn status_rows_say_why_there_is_no_pr() {
        let reason = |view: PrView, remote: Option<&RemoteRepo>| rows(view, remote, 80);
        let one = |text: &str| vec![("Pull request", text.to_string())];
        let none = found(None);
        assert_eq!(reason(PrView::Found { found: &none, error: None }, None), one("none  checked 2m ago"));
        let failed = error("timed out");
        assert_eq!(reason(PrView::Failed(&failed), None), one("failed 1m ago: timed out"));
        assert_eq!(reason(PrView::Unknown, None), one("looking up…"));
        assert_eq!(reason(PrView::Skipped(Skip::DefaultBranch), None), one("on the default branch"));
        assert_eq!(reason(PrView::Skipped(Skip::NotOnGitHub), None), one("not on GitHub"));
        let gitlab = RemoteRepo { host: "gitlab.com".into(), ..remote("acme") };
        assert_eq!(
            reason(PrView::Skipped(Skip::NotOnGitHub), Some(&gitlab)),
            one("not on GitHub (gitlab.com isn't a host gh is logged in to)")
        );
        assert_eq!(reason(PrView::GhNotInstalled, None), one("gh isn't installed (brew install gh)"));
        assert_eq!(reason(PrView::GhNotLoggedIn, None), one("gh isn't logged in: run gh auth login"));
        assert_eq!(reason(PrView::GhFailed("gh auth status timed out after 30s"), None), one("gh auth status timed out after 30s"));
        assert_eq!(reason(PrView::Off, None), vec![]);
    }

    #[test]
    fn the_column_hides_when_gh_cannot_answer() {
        assert!(column_shown(&GhStatus::Unchecked));
        assert!(column_shown(&GhStatus::Ready { hosts: vec!["github.com".into()] }));
        for gh in [GhStatus::Disabled, GhStatus::NotInstalled, GhStatus::NotLoggedIn, GhStatus::Failed("x".into())] {
            assert!(!column_shown(&gh), "{gh:?}");
        }
    }
}
