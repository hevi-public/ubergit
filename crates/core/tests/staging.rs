//! Line and hunk staging against real repos: diffs from `detail::file_diff`, patches from
//! `patch::Patch::build`, applied with `ops::apply_patch`. Each test has its own repo.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use async_io::block_on;
use tempfile::TempDir;
use ubergit_core::detail::{self, FileDiff};
use ubergit_core::git::parse;
use ubergit_core::ops::{self, PatchAction};
use ubergit_core::patch::Patch;
use ubergit_core::*;

const ISOLATED: [(&str, &str); 2] = [("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")];

fn git() -> Git {
    Git::new(Arc::new(())).with_env(ISOLATED)
}

struct Repo {
    _dir: TempDir,
    loc: RepoLocation,
}

impl Repo {
    /// A repo with one commit holding `files`.
    fn new(files: &[(&str, &[u8])]) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let loc = RepoLocation {
            root: root.clone(),
            name: "repo".into(),
            git_dir: root.join(".git"),
            common_dir: root.join(".git"),
            bare: false,
        };
        let repo = Repo { _dir: dir, loc };
        repo.run(&["init", "-q", "-b", "main"]);
        for (path, content) in files {
            repo.write(path, content);
        }
        repo.run(&["add", "-A"]);
        repo.run(&["commit", "-q", "--allow-empty", "-m", "init"]);
        repo
    }

    fn run(&self, args: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new("git")
            .current_dir(&self.loc.root)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .envs(ISOLATED)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn write(&self, path: &str, content: &[u8]) {
        std::fs::write(self.loc.root.join(path), content).unwrap();
    }

    fn worktree(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.loc.root.join(path)).ok()
    }

    /// The file's content in the index, `None` if it isn't there.
    fn index(&self, path: &str) -> Option<Vec<u8>> {
        let listed = self.run(&["ls-files", "--", path]);
        (!listed.is_empty()).then(|| self.run(&["show", &format!(":{path}")]))
    }

    fn file(&self, path: &str) -> FileEntry {
        let out = self.run(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        parse::status_v2(&out)
            .files
            .into_iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} has no changes"))
    }

    fn diff(&self, path: &str) -> FileDiff {
        block_on(detail::file_diff(&git(), &self.loc, &self.file(path))).unwrap()
    }

    /// Stages (or unstages, or discards) the changed lines of `path` whose text is in
    /// `lines`, which must be adjacent in the diff.
    fn apply(&self, path: &str, action: PatchAction, lines: &[&str]) {
        let diff = self.diff(path);
        let text = match action {
            PatchAction::Unstage => diff.staged,
            PatchAction::Stage | PatchAction::Discard => diff.unstaged,
        }
        .expect("a diff");
        let patch = Patch::parse(&text);
        let range = select(&text, lines);
        let built = patch
            .build(range, action != PatchAction::Stage)
            .unwrap()
            .expect("something selected");
        block_on(ops::apply_patch(&git(), &self.loc, built, action)).unwrap_or_else(|e| {
            panic!("{e}\n{}", e.details())
        });
    }
}

/// The lines of `diff` from the first to the last of `lines` (whole lines, e.g. `+new`).
fn select(diff: &[u8], lines: &[&str]) -> Range<usize> {
    let all: Vec<&[u8]> = diff.split(|&b| b == b'\n').collect();
    let position = |line: &str| {
        all.iter()
            .position(|l| *l == line.as_bytes())
            .unwrap_or_else(|| panic!("{line:?} not in\n{}", String::from_utf8_lossy(diff)))
    };
    let first = position(lines[0]);
    let last = position(lines[lines.len() - 1]);
    first..last + 1
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn stages_and_unstages_single_lines() {
    let repo = Repo::new(&[("f.txt", b"a\nb\nc\n")]);
    repo.write("f.txt", b"a\nB\nc\nd\n");
    repo.apply("f.txt", PatchAction::Stage, &["+d"]);
    assert_eq!(repo.index("f.txt").unwrap(), b"a\nb\nc\nd\n");
    repo.apply("f.txt", PatchAction::Stage, &["-b"]);
    assert_eq!(repo.index("f.txt").unwrap(), b"a\nc\nd\n");

    repo.apply("f.txt", PatchAction::Unstage, &["+d"]);
    assert_eq!(repo.index("f.txt").unwrap(), b"a\nc\n");
    assert_eq!(repo.worktree("f.txt").unwrap(), b"a\nB\nc\nd\n", "worktree untouched");
}

#[test]
fn discards_a_block_from_the_worktree_only() {
    let repo = Repo::new(&[("f.txt", b"a\nb\nc\n")]);
    repo.write("f.txt", b"a\nB\nc\nd\n");
    repo.apply("f.txt", PatchAction::Stage, &["+d"]);
    repo.apply("f.txt", PatchAction::Discard, &["-b", "+B"]);
    assert_eq!(repo.worktree("f.txt").unwrap(), b"a\nb\nc\nd\n");
    assert_eq!(repo.index("f.txt").unwrap(), b"a\nb\nc\nd\n", "index untouched");
}

#[test]
fn hunks_after_a_partly_staged_one_land_in_the_right_place() {
    let original = numbered(30);
    let repo = Repo::new(&[("f.txt", original.as_bytes())]);
    let changed = original
        .replace("line 2\n", "line 2\nnew A\nnew B\n")
        .replace("line 25\n", "line twenty-five\n");
    repo.write("f.txt", changed.as_bytes());

    // Only the second hunk, then one line of the first.
    repo.apply("f.txt", PatchAction::Stage, &["-line 25", "+line twenty-five"]);
    assert_eq!(repo.index("f.txt").unwrap(), original.replace("line 25\n", "line twenty-five\n").as_bytes());
    repo.apply("f.txt", PatchAction::Stage, &["+new B"]);
    let staged = original.replace("line 2\n", "line 2\nnew B\n").replace("line 25\n", "line twenty-five\n");
    assert_eq!(repo.index("f.txt").unwrap(), staged.as_bytes());

    // Unstage the second hunk while the first stays staged, then discard `+new A`.
    repo.apply("f.txt", PatchAction::Unstage, &["-line 25", "+line twenty-five"]);
    assert_eq!(repo.index("f.txt").unwrap(), original.replace("line 2\n", "line 2\nnew B\n").as_bytes());
    repo.apply("f.txt", PatchAction::Discard, &["+new A"]);
    assert_eq!(
        repo.worktree("f.txt").unwrap(),
        original.replace("line 2\n", "line 2\nnew B\n").replace("line 25\n", "line twenty-five\n").as_bytes()
    );
}

#[test]
fn stages_part_of_an_untracked_file() {
    let repo = Repo::new(&[]);
    repo.write("new file.txt", b"x\ny\nz\n");
    repo.apply("new file.txt", PatchAction::Stage, &["+y"]);
    assert_eq!(repo.index("new file.txt").unwrap(), b"y\n");
    assert_eq!(repo.file("new file.txt").short_status(), "AM");

    // The rest is now an ordinary unstaged change.
    repo.apply("new file.txt", PatchAction::Discard, &["+x"]);
    assert_eq!(repo.worktree("new file.txt").unwrap(), b"y\nz\n");
    repo.apply("new file.txt", PatchAction::Unstage, &["+y"]);
    assert_eq!(repo.index("new file.txt"), None, "unstaging every line removes it from the index");
}

#[test]
fn discards_part_or_all_of_an_untracked_file() {
    let repo = Repo::new(&[]);
    repo.write("u.txt", b"x\ny\nz\n");
    repo.apply("u.txt", PatchAction::Discard, &["+y"]);
    assert_eq!(repo.worktree("u.txt").unwrap(), b"x\nz\n");
    repo.apply("u.txt", PatchAction::Discard, &["+x", "+z"]);
    assert_eq!(repo.worktree("u.txt"), None, "discarding every line deletes the file");
}

#[test]
fn stages_part_of_a_deleted_file() {
    let repo = Repo::new(&[("gone.txt", b"a\nb\nc\n")]);
    std::fs::remove_file(repo.loc.root.join("gone.txt")).unwrap();
    repo.apply("gone.txt", PatchAction::Stage, &["-b"]);
    assert_eq!(repo.index("gone.txt").unwrap(), b"a\nc\n");
    repo.apply("gone.txt", PatchAction::Stage, &["-a", "-c"]);
    assert_eq!(repo.index("gone.txt"), None, "staging every line stages the deletion");
}

#[test]
fn unstages_a_staged_deletion_line_by_line() {
    let repo = Repo::new(&[("gone.txt", b"a\nb\nc\n")]);
    repo.run(&["rm", "-q", "gone.txt"]);
    repo.apply("gone.txt", PatchAction::Unstage, &["-b"]);
    assert_eq!(repo.index("gone.txt").unwrap(), b"b\n");
}

#[test]
fn unstaging_lines_of_a_rename_keeps_the_rename() {
    let repo = Repo::new(&[("old.txt", b"keep 1\nkeep 2\nkeep 3\nkeep 4\nx\n")]);
    repo.run(&["mv", "old.txt", "new.txt"]);
    repo.write("new.txt", b"keep 1\nkeep 2\nkeep 3\nkeep 4\ny\n");
    repo.run(&["add", "new.txt"]);
    let file = repo.file("new.txt");
    assert_eq!(file.kind, FileKind::Renamed);
    repo.apply("new.txt", PatchAction::Unstage, &["+y"]);
    assert_eq!(repo.index("new.txt").unwrap(), b"keep 1\nkeep 2\nkeep 3\nkeep 4\n");
    assert_eq!(repo.index("old.txt"), None);
}

#[test]
fn files_without_a_final_newline() {
    let repo = Repo::new(&[("f", b"a\nb")]);
    repo.write("f", b"a\nB");
    repo.apply("f", PatchAction::Stage, &["-b", "+B"]);
    assert_eq!(repo.index("f").unwrap(), b"a\nB");

    // Adding a line after `B` also adds B's newline, which is a change to `B`.
    repo.write("f", b"a\nB\nC\n");
    let diff = repo.diff("f").unstaged.unwrap();
    let range = select(&diff, &["+C"]);
    assert!(Patch::parse(&diff).build(range, false).is_err());
    repo.apply("f", PatchAction::Stage, &["-B", "+C"]);
    assert_eq!(repo.index("f").unwrap(), b"a\nB\nC\n");
}

#[test]
fn keeps_crlf_and_bytes_that_arent_utf8() {
    let repo = Repo::new(&[("w.txt", b"a\r\nb\r\n"), ("bin.txt", b"a\n\xff\n")]);
    repo.write("w.txt", b"a\r\nB\r\nc\r\n");
    repo.apply("w.txt", PatchAction::Stage, &["+c\r"]);
    assert_eq!(repo.index("w.txt").unwrap(), b"a\r\nb\r\nc\r\n");

    repo.write("bin.txt", b"a\n\xfe\n");
    let diff = repo.diff("bin.txt").unstaged.unwrap();
    let patch = Patch::parse(&diff);
    let first = patch.first_change().unwrap();
    let built = patch.build(patch.block(first).unwrap(), false).unwrap().unwrap();
    block_on(ops::apply_patch(&git(), &repo.loc, built, PatchAction::Stage)).unwrap();
    assert_eq!(repo.index("bin.txt").unwrap(), b"a\n\xfe\n");
}

#[test]
fn discards_lines_with_autocrlf() {
    // The diff shows the normalized LF content; the worktree keeps its CRLF.
    let repo = Repo::new(&[]);
    repo.run(&["config", "core.autocrlf", "true"]);
    repo.write("w.txt", b"a\r\nb\r\n");
    repo.run(&["add", "w.txt"]);
    repo.run(&["commit", "-q", "-m", "crlf"]);
    repo.write("w.txt", b"a\r\nB\r\nc\r\n");
    repo.apply("w.txt", PatchAction::Stage, &["+c"]);
    assert_eq!(repo.index("w.txt").unwrap(), b"a\nb\nc\n");
    repo.apply("w.txt", PatchAction::Discard, &["-b", "+B"]);
    assert_eq!(repo.worktree("w.txt").unwrap(), b"a\r\nb\r\nc\r\n");
}

#[test]
fn binary_files_have_no_lines_to_stage() {
    let repo = Repo::new(&[("img.bin", b"\x00\x01\x02")]);
    repo.write("img.bin", b"\x00\x01\x03");
    let diff = repo.diff("img.bin").unstaged.unwrap();
    assert!(!Patch::parse(&diff).has_changes(), "{}", String::from_utf8_lossy(&diff));
}

#[test]
fn stages_lines_in_a_repo_without_commits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = Repo {
        loc: RepoLocation {
            root: root.clone(),
            name: "unborn".into(),
            git_dir: root.join(".git"),
            common_dir: root.join(".git"),
            bare: false,
        },
        _dir: dir,
    };
    repo.run(&["init", "-q", "-b", "main"]);
    repo.write("f", b"1\n2\n");
    repo.apply("f", PatchAction::Stage, &["+2"]);
    assert_eq!(repo.index("f").unwrap(), b"2\n");
    repo.apply("f", PatchAction::Unstage, &["+2"]);
    assert_eq!(repo.index("f"), None);
    assert!(Path::new(&repo.loc.root.join("f")).exists());
}
