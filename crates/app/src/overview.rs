//! The overview, the main view while the Repos panel is focused: open pull requests rather
//! than the repos' state, which the Repos panel and the Status view show. Two tabs: the
//! selected repo's open PRs, teammates' included, and an inbox of every repo's, grouped by
//! what they want from the user.

use std::path::Path;

use ubergit_core::open_prs::{self, Checkout, Inbox, InboxPr, Listed, Listing, LocalRepo, OpenPr};
use ubergit_core::{Head, PrState};

use crate::pr;
use crate::store::{PrId, RepoStore};
use crate::text::{Line, age, truncate, truncate_middle};
use crate::theme::Palette;

pub const TABS: [&str; 2] = ["This repo", "Inbox"];

/// A PR's row: the PR, and what the detail below the list shows about it.
#[derive(Clone, Debug)]
pub struct PrRow {
    pub id: PrId,
    pub pr: OpenPr,
    /// Where its head is checked out, as the row says it.
    pub local: Option<String>,
}

/// One line of the overview. Only PR rows can be selected.
pub struct Row {
    pub line: Line,
    pub pr: Option<PrRow>,
}

impl Row {
    fn note(text: impl AsRef<str>) -> Self {
        let mut line = Line::new();
        line.color(text, Palette::dim());
        Self { line, pr: None }
    }

    fn line(line: Line) -> Self {
        Self { line, pr: None }
    }
}

/// The rows of tab `tab` ([`TABS`]), with PR titles cut so a row fits in `width` characters.
pub fn rows(store: &RepoStore, tab: usize, width: usize) -> Vec<Row> {
    // Every PR comes from gh: when it can't be asked, that's all there is to say.
    if let Some(problem) = pr::gh_problem(&store.gh_status) {
        return vec![Row::note(problem)];
    }
    if tab == 0 { this_repo(store, width) } else { inbox(store, width) }
}

/// A repo whose PRs are listed, and what's needed to tell where each is checked out.
struct Group<'a> {
    /// The main checkout's name, as the Repos panel has it.
    name: String,
    host: Option<&'a str>,
    local: Option<&'a LocalRepo>,
    /// Each worktree, the main checkout first, by name with its branch.
    checkouts: Vec<(String, Option<String>)>,
}

impl Group<'_> {
    fn of<'a>(store: &'a RepoStore, root: &Path) -> Option<Group<'a>> {
        let ix = store.index_of(root)?;
        let entry = &store.repos[ix];
        let branch = |ix: usize| {
            let summary = store.repos[ix].summary.as_ref()?;
            match &summary.head {
                Head::Branch(name) => Some(name.clone()),
                Head::Detached(_) | Head::Unborn(_) => None,
            }
        };
        let checkouts = std::iter::once(ix)
            .chain(store.worktrees_of(ix))
            .map(|ix| (store.repos[ix].short_name(), branch(ix)))
            .collect();
        let local = store.open.get(root).and_then(|open| open.local.as_ref());
        Some(Group {
            name: entry.short_name(),
            host: local.and_then(LocalRepo::github).map(|repo| repo.host.as_str()),
            local,
            checkouts,
        })
    }

    /// Where `pr`'s head is checked out: a worktree's name, or `local` for a branch no
    /// worktree has checked out.
    fn checkout(&self, pr: &OpenPr) -> Option<String> {
        let checkouts: Vec<(&str, Option<&str>)> =
            self.checkouts.iter().map(|(name, branch)| (name.as_str(), branch.as_deref())).collect();
        match open_prs::checkout(pr, self.local?, &checkouts)? {
            Checkout::Worktree(name) => Some(name.to_string()),
            Checkout::Local => Some("local".into()),
        }
    }
}

/// Tab 1: a header naming the repo on GitHub, then its open PRs, newest first.
fn this_repo(store: &RepoStore, width: usize) -> Vec<Row> {
    let Some(entry) = store.selected_entry() else {
        return vec![Row::note("No repository selected")];
    };
    let root = store.group_root(entry);
    let Some(group) = Group::of(store, root) else {
        return vec![Row::note("No repository selected")];
    };
    let open = store.open.get(root);
    let mut header = Line::new();
    let mut list = None;
    match open.and_then(|open| open.listing.as_ref()) {
        Some(Listing::Prs(prs)) => {
            header.bold(&prs.repo, Palette::fg());
            // "on GitHub": the Repos panel's `· N PR` counts only worktrees, so the two
            // numbers answer different questions and shouldn't read as a contradiction.
            header.color(format!("  {} open on GitHub", prs.total), Palette::cyan());
            if let Some(fork) = &prs.fork {
                header.color(format!("  (the parent of {fork})"), Palette::dim());
            }
            if let Some(checked) = open.and_then(|open| open.checked) {
                header.color(format!("  checked {} ago", age(Some(checked))), Palette::dim());
            }
            list = Some(prs);
        }
        Some(Listing::NoRemote) => {
            header.color("no remote", Palette::dim());
        }
        Some(Listing::NotOnGitHub(Some(repo))) => {
            header.color(format!("not on GitHub ({} isn't a host gh is logged in to)", repo.host), Palette::dim());
        }
        Some(Listing::NotOnGitHub(None)) => {
            header.color("not on GitHub", Palette::dim());
        }
        // Never kept: a failed lookup leaves the last answer.
        Some(Listing::Failed(_) | Listing::NotAsked) | None => {
            if open.is_none_or(|open| open.error.is_none()) {
                header.color("looking up…", Palette::dim());
            }
        }
    }
    if let Some((message, at)) = open.and_then(|open| open.error.as_ref()) {
        pr::failed_at(&mut header, message, *at, width);
    }
    let mut rows = vec![Row::line(header)];
    let Some(list) = list else { return rows };
    if list.prs.is_empty() {
        rows.push(Row::note("no open pull requests"));
        return rows;
    }
    let items: Vec<Item> = list
        .prs
        .iter()
        .map(|pr| Item {
            repo_name: None,
            repo: &list.repo,
            host: group.host,
            pr,
            local: group.checkout(pr),
        })
        .collect();
    let widths = Widths::of(&items, width);
    rows.extend(items.iter().map(|item| item.row(&widths)));
    if list.total as usize > list.prs.len() {
        rows.push(Row::note(format!("and {} more", list.total as usize - list.prs.len())));
    }
    rows
}

/// Tab 2: every repo's open PRs, grouped: waiting on the user's review, the user's own, and
/// teammates'. Each line starts with the repo's name.
fn inbox(store: &RepoStore, width: usize) -> Vec<Row> {
    let mains: Vec<&Path> = store
        .repos
        .iter()
        .filter(|entry| entry.main_repo.is_none())
        .map(|entry| entry.location.root.as_path())
        .collect();
    let groups: Vec<Option<Group>> = mains.iter().map(|root| Group::of(store, root)).collect();
    let listed: Vec<Listed> = mains
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            let Some(Listing::Prs(prs)) = store.open.get(*root)?.listing.as_ref() else {
                return None;
            };
            Some(Listed {
                group: i,
                host: groups[i].as_ref()?.host?,
                prs,
            })
        })
        .collect();
    if listed.is_empty() && store.open.values().all(|open| open.listing.is_none() && open.error.is_none()) {
        return vec![Row::note("looking up…")];
    }
    let requests: Vec<(&str, &[open_prs::ReviewRequest])> = store
        .open_hosts
        .iter()
        .filter_map(|(host, found)| Some((host.as_str(), found.review.as_deref()?)))
        .collect();
    let viewer = |host: &str| store.open_hosts.get(host)?.viewer.as_deref();
    let Inbox { review, yours, teammates } = open_prs::inbox(&listed, &requests, viewer);

    let sections: [(&str, Vec<Item>); 3] = [
        ("Waiting on your review", review.into_iter().map(|found| inbox_item(&groups, found)).collect()),
        ("Yours", yours.into_iter().map(|found| inbox_item(&groups, found)).collect()),
        ("Teammates'", teammates.into_iter().map(|found| inbox_item(&groups, found)).collect()),
    ];
    let all: Vec<&Item> = sections.iter().flat_map(|(_, items)| items).collect();
    let widths = Widths::of_refs(&all, width);
    let mut rows = Vec::new();
    // Say the scope once. Next to "This repo", the tab's name doesn't make clear that this
    // one spans the workdir rather than the selected repo.
    if !listed.is_empty() {
        let mut scope = Line::new();
        scope.color(
            match listed.len() {
                1 => "Open pull requests across 1 repository".to_string(),
                n => format!("Open pull requests across {n} repositories"),
            },
            Palette::dim(),
        );
        rows.push(Row::line(scope));
        rows.push(Row::line(Line::new()));
    }
    for (ix, (title, items)) in sections.iter().enumerate() {
        if ix > 0 {
            rows.push(Row::line(Line::new()));
        }
        let mut header = Line::new();
        header.bold(*title, Palette::fg());
        header.color(format!(" ({})", items.len()), Palette::dim());
        if ix == 0 {
            if let Some(message) = store.open_hosts.values().find_map(|host| host.review_error.as_deref()) {
                header.color(format!("  search failed: {}", truncate(message.lines().next().unwrap_or_default(), 60)), Palette::red());
            } else if requests.is_empty() {
                header.color("  looking up…", Palette::dim());
            }
        } else if ix == 1 && !store.open_hosts.values().any(|host| host.viewer.is_some()) {
            header.color("  (who's logged in isn't known yet)", Palette::dim());
        }
        rows.push(Row::line(header));
        rows.extend(items.iter().map(|item| item.row(&widths)));
    }

    // What the lists leave out.
    let more: Vec<String> = listed
        .iter()
        .filter(|list| list.prs.total as usize > list.prs.prs.len())
        .filter_map(|list| {
            let name = &groups[list.group].as_ref()?.name;
            Some(format!("{} in {name}", list.prs.total as usize - list.prs.prs.len()))
        })
        .collect();
    let failed: Vec<String> = mains
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            let open = store.open.get(*root)?;
            open.error.as_ref()?;
            Some(groups[i].as_ref()?.name.clone())
        })
        .collect();
    if !more.is_empty() || !failed.is_empty() {
        rows.push(Row::line(Line::new()));
    }
    if !more.is_empty() {
        rows.push(Row::note(truncate(&format!("Not shown, older: {}", more.join(", ")), width)));
    }
    if !failed.is_empty() {
        let mut line = Line::new();
        line.color(truncate(&format!("Couldn't look up: {}", failed.join(", ")), width), Palette::red());
        rows.push(Row::line(line));
    }
    rows
}

fn inbox_item<'a>(groups: &[Option<Group>], found: InboxPr<'a>) -> Item<'a> {
    let group = groups[found.group].as_ref();
    Item {
        repo_name: Some(group.map_or_else(String::new, |g| g.name.clone())),
        repo: found.repo,
        host: Some(found.host),
        pr: found.pr,
        local: group.and_then(|g| g.checkout(found.pr)),
    }
}

/// A PR to list, and where it's from.
struct Item<'a> {
    /// The local repo's name, in the inbox.
    repo_name: Option<String>,
    /// `owner/name` on GitHub.
    repo: &'a str,
    host: Option<&'a str>,
    pr: &'a OpenPr,
    local: Option<String>,
}

/// Column widths, in characters, so the rows line up.
struct Widths {
    repo_name: usize,
    number: usize,
    title: usize,
    author: usize,
    /// The checkout cell. Sized like the rest, and cut like the rest: worktree names run
    /// to 40 characters and would otherwise push the row past the pane.
    local: usize,
}

/// The widest a word can be (`approved`).
const WORD: usize = 8;
/// The widest an age can be, like `11M`.
const AGE: usize = 3;

impl Widths {
    fn of(items: &[Item], width: usize) -> Self {
        Self::of_refs(&items.iter().collect::<Vec<_>>(), width)
    }

    /// The title takes what the other columns leave, but no less than 20.
    fn of_refs(items: &[&Item], width: usize) -> Self {
        let max = |f: &dyn Fn(&Item) -> usize, cap: usize| items.iter().map(|item| f(item)).max().unwrap_or(0).min(cap);
        let repo_name = max(&|item| item.repo_name.as_deref().map_or(0, |n| n.chars().count()), 24);
        let number = max(&|item| format!("#{}", item.pr.number).len(), 8);
        let author = max(&|item| item.pr.author.as_deref().map_or(1, |a| a.chars().count()), 18);
        let local = max(&|item| item.local.as_deref().map_or(0, |l| l.chars().count()), 24);
        let gaps = |w: usize| if w > 0 { w + 2 } else { 0 };
        let fixed = gaps(repo_name) + number + 1 + WORD + 1 + 2 + 2 + author + 2 + AGE + 2 + local;
        Self {
            repo_name,
            number,
            title: width.saturating_sub(fixed).max(20),
            author,
            local,
        }
    }
}

impl Item<'_> {
    /// `api  #412 approved ✓ Round tax per line  alice  2h  api-fix`.
    fn row(&self, widths: &Widths) -> Row {
        let pr = self.pr;
        let mut line = Line::new();
        if let Some(name) = &self.repo_name {
            line.color(truncate(name, widths.repo_name), Palette::cyan());
            line.pad_to(widths.repo_name + 2);
        }
        let (word, color) = pr::word_for(if pr.draft { PrState::Draft } else { PrState::Open }, pr.review);
        let start = line.width();
        line.color(format!("#{}", pr.number), color);
        line.pad_to(start + widths.number + 1);
        line.color(word, color);
        line.pad_to(start + widths.number + 1 + WORD + 1);
        match pr.checks.map(pr::glyph_for) {
            Some((glyph, color)) => line.color(glyph, color),
            None => line.push(" "),
        };
        line.push(" ");
        let at = line.width();
        line.push(truncate(&pr.title, widths.title));
        line.pad_to(at + widths.title + 2);
        let at = line.width();
        match &pr.author {
            Some(author) => line.color(truncate(author, widths.author), Palette::blue()),
            None => line.color("-", Palette::dim()),
        };
        line.pad_to(at + widths.author + 2);
        let at = line.width();
        line.color(age(pr.updated), Palette::dim());
        line.pad_to(at + AGE + 2);
        match self.local.as_deref() {
            Some("local") => {
                line.color("local", Palette::dim());
            }
            Some(worktree) => {
                // Cut in the middle: agent worktrees differ only in a trailing hash.
                line.color(truncate_middle(worktree, widths.local), Palette::green());
            }
            None => {}
        }
        let id = self.host.map(|host| PrId {
            host: host.to_string(),
            repo: self.repo.to_string(),
            number: pr.number,
        });
        Row {
            line,
            pr: id.map(|id| PrRow {
                id,
                pr: pr.clone(),
                local: self.local.clone(),
            }),
        }
    }
}

/// The selected PR in more detail, below the list: its title in full, where it's from and
/// checked out, and, once it's been looked up in full, its reviewers and failing checks.
pub fn detail(store: &RepoStore, row: &PrRow, width: usize) -> Vec<Line> {
    let pr = &row.pr;
    let full = store.pr_details.get(&row.id);
    let mut lines = Vec::new();

    let (word, color) = pr::word_for(if pr.draft { PrState::Draft } else { PrState::Open }, pr.review);
    let mut title = Line::new();
    title.color(format!("#{} ", pr.number), color);
    title.bold(truncate(&pr.title, width.saturating_sub(20).max(20)), Palette::fg());
    title.color(format!("  {word}"), color);
    lines.push(title);

    let mut from = Line::new();
    from.color(&row.id.repo, Palette::dim());
    from.push("  ");
    let head = match pr.head_repo.as_deref() {
        Some(head) if !head.eq_ignore_ascii_case(&row.id.repo) => format!("{}:{}", head.split('/').next().unwrap_or(head), pr.head_ref),
        _ => pr.head_ref.clone(),
    };
    from.color(truncate(&head, 50), Palette::blue());
    if let Some(Ok(full)) = full.map(|full| &full.result) {
        from.color(format!(" → {}", full.base_ref), Palette::blue());
    }
    from.color(
        format!("  by {}, updated {} ago", pr.author.as_deref().unwrap_or("a deleted account"), age(pr.updated)),
        Palette::dim(),
    );
    match row.local.as_deref() {
        Some("local") => {
            from.color("  (a local branch has it)", Palette::dim());
        }
        Some(worktree) => {
            from.color(format!("  checked out in {worktree}"), Palette::green());
        }
        None => {}
    }
    lines.push(from);

    let kv = |key: &str, value: Line| {
        let mut line = Line::new();
        line.color(format!("{key:<10}"), Palette::dim());
        line.append(value);
        line
    };
    let value_width = width.saturating_sub(10);
    match full.map(|full| (&full.result, full.checked)) {
        Some((Ok(full), _)) => {
            let reviews = if full.reviewers.is_empty() {
                let mut none = Line::new();
                none.color("none asked for", Palette::dim());
                none
            } else {
                pr::reviews(&full.reviewers, value_width)
            };
            lines.push(kv("Reviews", reviews));
            let checks = match &full.checks {
                Some(checks) => pr::checks_line(checks, value_width),
                None => {
                    let mut none = Line::new();
                    none.color("none", Palette::dim());
                    none
                }
            };
            lines.push(kv("Checks", checks));
        }
        Some((Err(message), at)) => {
            let mut failed = Line::new();
            pr::failed_at(&mut failed, message, at, value_width);
            lines.push(kv("Details", failed));
        }
        None => {
            let mut loading = Line::new();
            loading.color("looking up…", Palette::dim());
            lines.push(kv("Reviews", loading.clone()));
            let checks = match pr.checks {
                Some(state) => {
                    let (glyph, color) = pr::glyph_for(state);
                    let mut line = Line::new();
                    line.color(glyph, color);
                    line.color("  looking up which…", Palette::dim());
                    line
                }
                None => loading,
            };
            lines.push(kv("Checks", checks));
        }
    }

    let mut url = Line::new();
    url.color(truncate(&pr.url, value_width), Palette::dim());
    url.color("  G opens it", Palette::blue());
    lines.push(kv("Link", url));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use ubergit_core::ReviewDecision;

    fn text(line: &Line) -> String {
        line.spans().into_iter().map(|(text, _)| text).collect()
    }

    fn open_pr(number: u64, title: &str, author: Option<&str>) -> OpenPr {
        OpenPr {
            number,
            title: title.into(),
            url: format!("https://github.com/team/api/pull/{number}"),
            author: author.map(Into::into),
            draft: false,
            review: Some(ReviewDecision::Approved),
            checks: Some(ubergit_core::ChecksState::Failing),
            updated: Some(SystemTime::now() - Duration::from_secs(2 * 3600)),
            head_ref: "fix".into(),
            head_repo: Some("team/api".into()),
        }
    }

    #[test]
    fn rows_line_up_and_cut_the_title_to_fit() {
        let (a, b) = (open_pr(7, "Round tax per line item", Some("alice")), open_pr(1234, "Fix", None));
        let items = [
            Item { repo_name: Some("api".into()), repo: "team/api", host: Some("github.com"), pr: &a, local: Some("api-fix".into()) },
            Item { repo_name: Some("billing".into()), repo: "team/billing", host: None, pr: &b, local: None },
        ];
        let widths = Widths::of(&items, 70);
        let rows: Vec<Row> = items.iter().map(|item| item.row(&widths)).collect();
        assert_eq!(text(&rows[0].line), "api      #7    approved ✗ Round tax per line item  alice  2h   api-fix");
        assert_eq!(text(&rows[1].line), "billing  #1234 approved ✗ Fix                      -      2h   ");
        assert_eq!(text(&rows[0].line).chars().count(), 70);
        let narrow = Widths::of(&items, 67);
        assert_eq!(text(&items[0].row(&narrow).line), "api      #7    approved ✗ Round tax per line …  alice  2h   api-fix");
        let id = &rows[0].pr.as_ref().unwrap().id;
        assert_eq!((id.host.as_str(), id.repo.as_str(), id.number), ("github.com", "team/api", 7));
        // Without a host, it can't be selected.
        assert!(rows[1].pr.is_none());
        // The title never gets narrower than 20.
        assert_eq!(Widths::of(&items, 10).title, 20);
    }
}
