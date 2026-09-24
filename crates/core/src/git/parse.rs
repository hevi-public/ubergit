//! Parsers for machine-readable git output. Pure functions, unit-tested with fixtures.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::*;

/// Field separator used in our `--format` strings (`%x1f`, ASCII unit separator).
pub const FS: char = '\x1f';
/// Record separator used in our `--format` strings (`%x1e`, ASCII record separator).
pub const RS: char = '\x1e';

pub fn unix_time(secs: &str) -> Option<SystemTime> {
    let secs: u64 = secs.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// `git status --porcelain=v2 --branch --show-stash -z`
pub fn status_v2(out: &[u8]) -> StatusReport {
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0');
    let mut report = StatusReport::default();

    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => report.oid = (value != "(initial)").then(|| value.to_string()),
                "branch.head" => report.branch = (value != "(detached)").then(|| value.to_string()),
                "branch.upstream" => report.upstream = Some(value.to_string()),
                "branch.ab" => {
                    let mut parts = value.split_whitespace();
                    let ahead = parts.next().and_then(|a| a.trim_start_matches('+').parse().ok());
                    let behind = parts.next().and_then(|b| b.trim_start_matches('-').parse().ok());
                    if let (Some(ahead), Some(behind)) = (ahead, behind) {
                        report.ahead_behind = Some((ahead, behind));
                    }
                }
                "stash" => report.stash_count = value.trim().parse().unwrap_or(0),
                _ => {}
            }
            continue;
        }

        let (tag, rest) = record.split_at(1);
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        match tag {
            // 1 XY sub mH mI mW hH hI path
            "1" => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                if let [xy, _, _, _, _, _, _, path] = parts[..] {
                    report.files.push(entry(xy, path, None, FileKind::Tracked));
                }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            "2" => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                let orig = fields.next().map(str::to_string);
                if let [xy, _, _, _, _, _, _, _, path] = parts[..] {
                    report.files.push(entry(xy, path, orig, FileKind::Renamed));
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => {
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                if let [xy, _, _, _, _, _, _, _, _, path] = parts[..] {
                    report.files.push(entry(xy, path, None, FileKind::Unmerged));
                }
            }
            "?" => report.files.push(FileEntry {
                path: rest.to_string(),
                orig_path: None,
                index: '?',
                worktree: '?',
                kind: FileKind::Untracked,
            }),
            _ => {}
        }
    }
    report
}

fn entry(xy: &str, path: &str, orig_path: Option<String>, kind: FileKind) -> FileEntry {
    let mut chars = xy.chars();
    FileEntry {
        path: path.to_string(),
        orig_path,
        index: chars.next().unwrap_or('.'),
        worktree: chars.next().unwrap_or('.'),
        kind,
    }
}

/// `%(upstream:track,nobracket)`: `ahead 1, behind 2` / `ahead 1` / `gone` / empty.
pub fn upstream_track(upstream: &str, track: &str) -> Upstream {
    if upstream.is_empty() {
        return Upstream::None;
    }
    if track.trim() == "gone" {
        return Upstream::Gone {
            name: upstream.to_string(),
        };
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => ahead = n,
            (Some("behind"), Some(n)) => behind = n,
            _ => {}
        }
    }
    Upstream::Tracking {
        name: upstream.to_string(),
        ahead,
        behind,
    }
}

/// Format for [`branches`]; `base` is the full default-branch ref, if any.
pub fn branch_format(base: Option<&str>) -> String {
    let ahead_behind = base
        .map(|b| format!("%(ahead-behind:{b})"))
        .unwrap_or_default();
    [
        "%(HEAD)",
        "%(refname:short)",
        "%(objectname:short)",
        "%(upstream:short)",
        "%(upstream:track,nobracket)",
        "%(committerdate:unix)",
        "%(subject)",
        ahead_behind.as_str(),
        "%(worktreepath)",
    ]
    .join("%1f")
        + "%1e"
}

/// `git for-each-ref --format=<branch_format> refs/heads`
pub fn branches(out: &str, current_worktree: Option<&std::path::Path>) -> Vec<Branch> {
    records(out)
        .filter_map(|fields| {
            let [head, name, oid, upstream, track, date, subject, ahead_behind, worktree] =
                fields[..]
            else {
                return None;
            };
            let base = {
                let mut it = ahead_behind.split_whitespace().map(|n| n.parse::<u32>().ok());
                match (it.next().flatten(), it.next().flatten()) {
                    (Some(a), Some(b)) => Some((a, b)),
                    _ => None,
                }
            };
            let worktree = Some(PathBuf::from(worktree))
                .filter(|p| !p.as_os_str().is_empty() && Some(p.as_path()) != current_worktree);
            Some(Branch {
                name: name.to_string(),
                is_head: head == "*",
                short_oid: oid.to_string(),
                upstream: upstream_track(upstream, track),
                base,
                committed: unix_time(date),
                subject: subject.to_string(),
                worktree,
            })
        })
        .collect()
}

/// Splits `%1e`-terminated records into `%1f`-separated fields.
pub fn records(out: &str) -> impl Iterator<Item = Vec<&str>> {
    out.split(RS)
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .map(|r| r.split(FS).collect())
}

pub const REMOTE_BRANCH_FORMAT: &str = "%(refname)%1f%(objectname:short)%1f%(subject)%1e";

/// `git for-each-ref --format=REMOTE_BRANCH_FORMAT refs/remotes`
pub fn remote_branches(out: &str) -> Vec<RemoteBranch> {
    records(out)
        .filter_map(|fields| {
            let [refname, oid, subject] = fields[..] else {
                return None;
            };
            let short = refname.strip_prefix("refs/remotes/")?;
            let (remote, name) = short.split_once('/')?;
            if name == "HEAD" {
                return None;
            }
            Some(RemoteBranch {
                remote: remote.to_string(),
                name: name.to_string(),
                short_oid: oid.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

pub const TAG_FORMAT: &str = "%(refname:short)%1f%(objectname:short)%1f%(subject)%1e";

/// `git for-each-ref --sort=-creatordate --format=TAG_FORMAT refs/tags`
pub fn tags(out: &str) -> Vec<Tag> {
    records(out)
        .filter_map(|fields| {
            let [name, oid, subject] = fields[..] else {
                return None;
            };
            Some(Tag {
                name: name.to_string(),
                short_oid: oid.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// `git remote -v`
pub fn remotes(out: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !remotes.iter().any(|r| r.name == name) {
            remotes.push(Remote {
                name: name.to_string(),
                url: url.to_string(),
            });
        }
    }
    remotes
}

pub const LOG_FORMAT: &str = "%H%x1f%h%x1f%an%x1f%ct%x1f%D%x1f%s%x1e";

/// `git log --format=LOG_FORMAT`
pub fn log(out: &str) -> Vec<Commit> {
    records(out)
        .filter_map(|fields| {
            let [oid, short, author, time, refs, subject] = fields[..] else {
                return None;
            };
            Some(Commit {
                oid: oid.to_string(),
                short_oid: short.to_string(),
                author: author.to_string(),
                time: unix_time(time),
                subject: subject.to_string(),
                refs: refs.to_string(),
            })
        })
        .collect()
}

pub const REFLOG_FORMAT: &str = "%h%x1f%gd%x1f%gs%x1f%ct%x1e";

/// `git reflog --format=REFLOG_FORMAT`
pub fn reflog(out: &str) -> Vec<ReflogEntry> {
    records(out)
        .filter_map(|fields| {
            let [short, selector, subject, time] = fields[..] else {
                return None;
            };
            Some(ReflogEntry {
                short_oid: short.to_string(),
                selector: selector.to_string(),
                subject: subject.to_string(),
                time: unix_time(time),
            })
        })
        .collect()
}

pub const STASH_FORMAT: &str = "%gd%x1f%ct%x1f%gs%x1e";

/// `git stash list --format=STASH_FORMAT`
pub fn stashes(out: &str) -> Vec<StashEntry> {
    records(out)
        .filter_map(|fields| {
            let [selector, time, subject] = fields[..] else {
                return None;
            };
            let index = selector
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(StashEntry {
                index,
                subject: subject.to_string(),
                time: unix_time(time),
            })
        })
        .collect()
}

/// `git worktree list --porcelain -z`
pub fn worktrees(out: &[u8], current: &std::path::Path) -> Vec<Worktree> {
    let text = String::from_utf8_lossy(out);
    let mut result = Vec::new();
    let mut current_wt: Option<Worktree> = None;
    for field in text.split('\0') {
        if field.is_empty() {
            if let Some(wt) = current_wt.take() {
                result.push(wt);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        match key {
            "worktree" => {
                if let Some(wt) = current_wt.take() {
                    result.push(wt);
                }
                let path = PathBuf::from(value);
                current_wt = Some(Worktree {
                    is_current: path == current,
                    path,
                    head: None,
                    branch: None,
                    bare: false,
                });
            }
            "HEAD" => {
                if let Some(wt) = current_wt.as_mut() {
                    wt.head = Some(value.to_string());
                }
            }
            "branch" => {
                if let Some(wt) = current_wt.as_mut() {
                    wt.branch = Some(value.trim_start_matches("refs/heads/").to_string());
                }
            }
            "bare" => {
                if let Some(wt) = current_wt.as_mut() {
                    wt.bare = true;
                }
            }
            _ => {}
        }
    }
    result.extend(current_wt);
    result
}

/// `git submodule status`
pub fn submodules(out: &str) -> Vec<Submodule> {
    out.lines()
        .filter(|l| l.len() > 1)
        .filter_map(|line| {
            let state = line.chars().next()?;
            let mut parts = line[1..].split_whitespace();
            let oid = parts.next()?;
            let path = parts.next()?;
            Some(Submodule {
                path: path.to_string(),
                short_oid: oid.chars().take(7).collect(),
                state,
            })
        })
        .collect()
}

/// `git rev-list --left-right --count A...B` → `(left, right)`.
pub fn left_right(out: &str) -> Option<(u32, u32)> {
    let mut parts = out.split_whitespace().map(|n| n.parse().ok());
    Some((parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_with_upstream_rename_untracked_and_conflict() {
        let out = b"# branch.oid e68950e7\0# branch.head feat\0# branch.upstream origin/main\0\
# branch.ab +1 -2\0# stash 3\0\
1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
2 RM N... 100644 100644 100644 aaa bbb R66 g\0f\0\
u UU N... 100644 100644 100644 100644 a b c conflict.txt\0\
? new file.txt\0";
        let report = status_v2(out);
        assert_eq!(report.oid.as_deref(), Some("e68950e7"));
        assert_eq!(report.head(), Head::Branch("feat".into()));
        assert_eq!(
            report.upstream(),
            Upstream::Tracking { name: "origin/main".into(), ahead: 1, behind: 2 }
        );
        assert_eq!(report.stash_count, 3);
        assert_eq!(report.files.len(), 4);
        assert_eq!(report.files[0].path, "src/a b.rs");
        assert!(!report.files[0].has_staged() && report.files[0].has_unstaged());
        assert_eq!(report.files[1].path, "g");
        assert_eq!(report.files[1].orig_path.as_deref(), Some("f"));
        assert!(report.files[1].has_staged() && report.files[1].has_unstaged());
        assert_eq!(report.files[2].kind, FileKind::Unmerged);
        assert_eq!(report.files[3].path, "new file.txt");
        assert_eq!(
            report.change_counts(),
            ChangeCounts { files: 4, staged: 1, unstaged: 2, untracked: 1, conflicted: 1 }
        );
    }

    #[test]
    fn status_detached_unborn_gone() {
        let detached = status_v2(b"# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(detached.head(), Head::Detached("abc".into()));
        let unborn = status_v2(b"# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(unborn.head(), Head::Unborn("main".into()));
        let gone = status_v2(b"# branch.oid abc\0# branch.head x\0# branch.upstream origin/x\0");
        assert_eq!(gone.upstream(), Upstream::Gone { name: "origin/x".into() });
    }

    #[test]
    fn track() {
        assert_eq!(upstream_track("", ""), Upstream::None);
        assert_eq!(upstream_track("o/x", "gone"), Upstream::Gone { name: "o/x".into() });
        assert_eq!(
            upstream_track("o/x", "ahead 3, behind 4"),
            Upstream::Tracking { name: "o/x".into(), ahead: 3, behind: 4 }
        );
        assert_eq!(
            upstream_track("o/x", "behind 4"),
            Upstream::Tracking { name: "o/x".into(), ahead: 0, behind: 4 }
        );
    }

    #[test]
    fn branch_records() {
        let out = "*\x1fmain\x1fabc\x1forigin/main\x1f\x1f1700000000\x1finit\x1f0 0\x1f/r\x1e\n \
\x1ffeat\x1fdef\x1f\x1f\x1f1700000001\x1fwip: a, b\x1f2 5\x1f/other\x1e\n";
        let branches = branches(out, Some(std::path::Path::new("/r")));
        assert_eq!(branches.len(), 2);
        assert!(branches[0].is_head);
        assert_eq!(branches[0].worktree, None);
        assert_eq!(branches[1].base, Some((2, 5)));
        assert_eq!(branches[1].subject, "wip: a, b");
        assert_eq!(branches[1].worktree, Some(PathBuf::from("/other")));
    }

    #[test]
    fn stash_and_log() {
        let stashes = stashes("stash@{0}\x1f1700000000\x1fWIP on main: abc x\x1e\nstash@{1}\x1f1\x1fOn b: y\x1e\n");
        assert_eq!(stashes.len(), 2);
        assert_eq!(stashes[1].index, 1);
        let log = log("abcdef\x1fabc\x1fMe\x1f1700000000\x1fHEAD -> main\x1fsubject\x1e\n");
        assert_eq!(log[0].refs, "HEAD -> main");
    }

    #[test]
    fn worktree_list() {
        let out = b"worktree /a\0HEAD abc\0branch refs/heads/main\0\0worktree /b\0HEAD def\0detached\0\0";
        let wts = worktrees(out, std::path::Path::new("/b"));
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert!(wts[1].is_current && wts[1].branch.is_none());
    }
}
