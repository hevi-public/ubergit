//! Staging, unstaging and discarding single lines or hunks, as in lazygit's staging view.
//!
//! [`Patch::parse`] reads the `git diff` of a file. [`Patch::build`] turns the selected
//! lines into a smaller patch for `git apply`. Lines outside the selection stay as they
//! are in the file being patched:
//! - applied forwards (staging, onto the index), an unselected `-` line becomes context and
//!   an unselected `+` line is dropped;
//! - applied in reverse (unstaging from the index, or discarding from the worktree), an
//!   unselected `+` line becomes context and an unselected `-` line is dropped.
//!
//! Everything works on bytes, so content that isn't UTF-8 is staged unchanged.

use std::ops::Range;

/// A parsed `git diff`. Line numbers are indices into the diff's lines (split on `\n`),
/// the same lines the main view shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    lines: Vec<Vec<u8>>,
    /// Whether each line is a `+` or `-` line that can be staged on its own.
    changes: Vec<bool>,
    files: Vec<FileSection>,
}

/// One `diff --git` section. There's usually one per diff, but a rename git didn't pair
/// up shows as a deletion and an addition.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileSection {
    /// From the `diff --git` line up to the first hunk.
    header: Range<usize>,
    hunks: Vec<Hunk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Hunk {
    /// The `@@` line.
    at: usize,
    /// One past the hunk's last line, including any `\ No newline at end of file`.
    end: usize,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    /// Whatever follows the closing `@@` (usually the enclosing function).
    heading: Vec<u8>,
}

/// The selection can't be applied on its own because of a missing newline at the end of
/// the file (e.g. adding a line after a last line that has no newline).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "The last line of this file has no newline, so this change can't be split there. Select the lines next to it as well (or the whole hunk)."
)]
pub struct NoNewlineConflict;

impl Patch {
    pub fn parse(diff: &[u8]) -> Patch {
        let mut lines: Vec<Vec<u8>> = diff.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
        if lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        let mut changes = vec![false; lines.len()];
        let mut files = Vec::new();
        let mut ix = 0;
        while ix < lines.len() {
            // Combined diffs of conflicted files (`diff --cc`) aren't stageable.
            if !lines[ix].starts_with(b"diff --git ") {
                ix += 1;
                continue;
            }
            let start = ix;
            ix += 1;
            while ix < lines.len() && !lines[ix].starts_with(b"@@ ") && !lines[ix].starts_with(b"diff ") {
                ix += 1;
            }
            let header = start..ix;
            let mut hunks = Vec::new();
            while let Some(hunk) = lines.get(ix).and_then(|line| parse_hunk(&lines, ix, line)) {
                ix = hunk.end;
                hunks.push(hunk);
            }
            // A submodule's "hunk" is a commit id: stage or discard it as a whole file.
            let submodule = lines[header.clone()].iter().any(|l| is_gitlink(l));
            if !submodule {
                for hunk in &hunks {
                    for line in hunk.at + 1..hunk.end {
                        changes[line] = matches!(lines[line].first(), Some(b'+' | b'-'));
                    }
                }
            }
            files.push(FileSection { header, hunks });
        }
        Patch { lines, changes, files }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether line `ix` is a `+` or `-` line that can be staged.
    pub fn is_change(&self, ix: usize) -> bool {
        self.changes.get(ix).copied().unwrap_or(false)
    }

    pub fn has_changes(&self) -> bool {
        self.changes.contains(&true)
    }

    pub fn first_change(&self) -> Option<usize> {
        self.changes.iter().position(|&c| c)
    }

    /// The first changed line at or after `ix`, else the last one before it.
    pub fn nearest_change(&self, ix: usize) -> Option<usize> {
        let ix = ix.min(self.len());
        (ix..self.len())
            .find(|&i| self.changes[i])
            .or_else(|| (0..ix).rev().find(|&i| self.changes[i]))
    }

    /// The next (or previous) changed line after (or before) `ix`.
    pub fn adjacent_change(&self, ix: usize, forward: bool) -> Option<usize> {
        if forward {
            (ix + 1..self.len()).find(|&i| self.changes[i])
        } else {
            (0..ix.min(self.len())).rev().find(|&i| self.changes[i])
        }
    }

    /// The run of adjacent changed lines around `ix`, lazygit's "hunk" in hunk mode. It
    /// includes the `\ No newline at end of file` markers among them.
    pub fn block(&self, ix: usize) -> Option<Range<usize>> {
        if !self.is_change(ix) {
            return None;
        }
        let part = |i: usize| self.changes[i] || self.lines[i].starts_with(b"\\");
        let mut start = ix;
        while start > 0 && part(start - 1) {
            start -= 1;
        }
        let mut end = ix + 1;
        while end < self.len() && part(end) {
            end += 1;
        }
        Some(start..end)
    }

    /// The first line of the next (or previous) block of changes.
    pub fn adjacent_block(&self, ix: usize, forward: bool) -> Option<usize> {
        let current = self.block(ix).unwrap_or(ix..ix + 1);
        if forward {
            (current.end..self.len()).find(|&i| self.changes[i])
        } else {
            let previous = (0..current.start.min(self.len())).rev().find(|&i| self.changes[i])?;
            self.block(previous).map(|block| block.start)
        }
    }

    /// A patch of just the changed lines in `selected`, to be applied forwards (`git apply
    /// --cached`) or in reverse (`git apply -R`, with `--cached` to unstage). `None` when
    /// the selection has no changed lines.
    pub fn build(&self, selected: Range<usize>, reverse: bool) -> Result<Option<Vec<u8>>, NoNewlineConflict> {
        let is_selected = |ix: usize| selected.contains(&ix) && self.changes[ix];
        let mut out = Vec::new();
        for file in &self.files {
            let mut body = Vec::new();
            // Lines added minus lines removed by the hunks written so far.
            let mut delta: i64 = 0;
            let (mut old_total, mut new_total) = (0u32, 0u32);
            for hunk in &file.hunks {
                if !(hunk.at + 1..hunk.end).any(is_selected) {
                    continue;
                }
                let (lines, old, new) = self.hunk_lines(hunk, &is_selected, reverse)?;
                // The hunk's first line in each file (a header gives one less for an empty
                // side). The side the patch is applied to is unchanged; the other side
                // moves by what the earlier hunks added or removed.
                let first = |start: u32, count: u32| i64::from(start) + i64::from(count == 0);
                let (old_first, new_first) = if reverse {
                    let new_first = first(hunk.new_start, hunk.new_count);
                    (new_first - delta, new_first)
                } else {
                    let old_first = first(hunk.old_start, hunk.old_count);
                    (old_first, old_first + delta)
                };
                let start = |first: i64, count: u32| first - i64::from(count == 0);
                body.extend(
                    format!("@@ -{},{old} +{},{new} @@", start(old_first, old), start(new_first, new)).bytes(),
                );
                body.extend(&hunk.heading);
                body.push(b'\n');
                body.extend(lines);
                delta += i64::from(new) - i64::from(old);
                old_total += old;
                new_total += new;
            }
            if !body.is_empty() {
                out.extend(self.header(file, old_total, new_total));
                out.extend(body);
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }

    /// A hunk's body with only the selected changes, and its old and new line counts.
    fn hunk_lines(
        &self,
        hunk: &Hunk,
        is_selected: &impl Fn(usize) -> bool,
        reverse: bool,
    ) -> Result<(Vec<u8>, u32, u32), NoNewlineConflict> {
        let mut out = Vec::new();
        let (mut old, mut new) = (0u32, 0u32);
        // What the last line written was, and whether the last line read was dropped.
        let mut last = None;
        let mut dropped = false;
        // Set by `\ No newline at end of file`: that side of the hunk can't go on.
        let (mut old_ended, mut new_ended) = (false, false);
        for ix in hunk.at + 1..hunk.end {
            let line = &self.lines[ix];
            let (kind, text) = match line.split_first() {
                Some((&b'\\', _)) => {
                    if !dropped {
                        match last {
                            Some(b'-') => old_ended = true,
                            Some(b'+') => new_ended = true,
                            _ => (old_ended, new_ended) = (true, true),
                        }
                        out.extend(line);
                        out.push(b'\n');
                    }
                    continue;
                }
                Some((&kind @ (b'+' | b'-'), text)) => {
                    let kept_as_context = if kind == b'+' { reverse } else { !reverse };
                    if is_selected(ix) {
                        (kind, text)
                    } else if kept_as_context {
                        (b' ', text)
                    } else {
                        dropped = true;
                        continue;
                    }
                }
                Some((_, text)) => (b' ', text),
                // An empty context line (`diff.suppressBlankEmpty`).
                None => (b' ', &line[..]),
            };
            dropped = false;
            let ended = match kind {
                b'-' => old_ended,
                b'+' => new_ended,
                _ => old_ended || new_ended,
            };
            if ended {
                return Err(NoNewlineConflict);
            }
            match kind {
                b'-' => old += 1,
                b'+' => new += 1,
                _ => (old, new) = (old + 1, new + 1),
            }
            last = Some(kind);
            out.push(kind);
            out.extend(text);
            out.push(b'\n');
        }
        Ok((out, old, new))
    }

    /// The file's header, rewritten as a plain change to one file where a partial patch
    /// can't keep it: a new file that the patch doesn't wholly remove when reversed, a
    /// deleted file that isn't wholly deleted, or a rename (only the content is staged).
    fn header(&self, file: &FileSection, old_total: u32, new_total: u32) -> Vec<u8> {
        let header = &self.lines[file.header.clone()];
        let find = |prefix: &[u8]| header.iter().find(|l| l.starts_with(prefix));
        let (minus, plus) = (find(b"--- "), find(b"+++ "));
        let created = minus.is_some_and(|l| l.as_slice() == b"--- /dev/null");
        let deleted = plus.is_some_and(|l| l.as_slice() == b"+++ /dev/null");
        let renamed = header.iter().any(|l| l.starts_with(b"rename from ") || l.starts_with(b"copy from "));
        let rewrite = renamed || created && old_total > 0 || deleted && new_total > 0;
        let path = if deleted { minus } else { plus };
        let mut out = Vec::new();
        match path.filter(|_| rewrite) {
            Some(path) => {
                // `+++ b/name` (git adds a tab after names with spaces; it's quoted when it
                // has special characters).
                let name = &path[4..];
                let bare = name.strip_suffix(b"\t").unwrap_or(name);
                let line = |parts: &[&[u8]]| {
                    let mut line = parts.concat();
                    line.push(b'\n');
                    line
                };
                out.extend(line(&[b"diff --git ", &with_side(bare, b'a'), b" ", &with_side(bare, b'b')]));
                for l in header.iter().filter(|l| l.starts_with(b"old mode ") || l.starts_with(b"new mode ")) {
                    out.extend(line(&[l]));
                }
                out.extend(line(&[b"--- ", &with_side(name, b'a')]));
                out.extend(line(&[b"+++ ", &with_side(name, b'b')]));
            }
            None => {
                for l in header {
                    out.extend(l);
                    out.push(b'\n');
                }
            }
        }
        out
    }
}

/// Parses the hunk whose `@@` line is `lines[at]`. `None` if it isn't one, or if the diff
/// ends before the hunk does.
fn parse_hunk(lines: &[Vec<u8>], at: usize, line: &[u8]) -> Option<Hunk> {
    let rest = line.strip_prefix(b"@@ -")?;
    let close = rest.windows(3).position(|w| w == b" @@")?;
    let ranges = std::str::from_utf8(&rest[..close]).ok()?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let ((old_start, old_count), (new_start, new_count)) = (range(old)?, range(new)?);
    let (mut old_left, mut new_left) = (old_count, new_count);
    let mut ix = at + 1;
    while old_left > 0 || new_left > 0 {
        let line = lines.get(ix)?;
        match line.first() {
            Some(b'-') if old_left > 0 => old_left -= 1,
            Some(b'+') if new_left > 0 => new_left -= 1,
            Some(b'\\') => {}
            Some(b' ') | None if old_left > 0 && new_left > 0 => (old_left, new_left) = (old_left - 1, new_left - 1),
            _ => return None,
        }
        ix += 1;
    }
    while lines.get(ix).is_some_and(|l| l.starts_with(b"\\")) {
        ix += 1;
    }
    Some(Hunk {
        at,
        end: ix,
        old_start,
        old_count,
        new_start,
        new_count,
        heading: rest[close + 3..].to_vec(),
    })
}

/// A header line of a submodule (gitlink, mode 160000).
fn is_gitlink(line: &[u8]) -> bool {
    line.ends_with(b"mode 160000") || line.starts_with(b"index ") && line.ends_with(b" 160000")
}

/// `b/name` (or `"b/name"`) with its `a/` or `b/` prefix set to `side`.
fn with_side(name: &[u8], side: u8) -> Vec<u8> {
    let mut name = name.to_vec();
    let at = usize::from(name.first() == Some(&b'"'));
    if matches!(name.get(at..at + 2), Some([b'a' | b'b', b'/'])) {
        name[at] = side;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(diff: &str, selected: Range<usize>, reverse: bool) -> String {
        let patch = Patch::parse(diff.as_bytes());
        let out = patch.build(selected, reverse).unwrap().expect("a patch");
        String::from_utf8(out).unwrap()
    }

    const MODIFIED: &str = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,6 +1,6 @@ fn top()
 one
-two
+TWO
 three
 four
-five
+FIVE
 six
@@ -20,3 +20,4 @@
 twenty
+new
 twenty-one
 twenty-two
";

    #[test]
    fn finds_changes_and_blocks() {
        let patch = Patch::parse(MODIFIED.as_bytes());
        assert_eq!(patch.len(), 18);
        let changes: Vec<usize> = (0..patch.len()).filter(|&i| patch.is_change(i)).collect();
        assert_eq!(changes, [6, 7, 10, 11, 15]);
        assert_eq!(patch.first_change(), Some(6));
        assert_eq!(patch.nearest_change(8), Some(10));
        assert_eq!(patch.nearest_change(17), Some(15));
        assert_eq!(patch.block(7), Some(6..8));
        assert_eq!(patch.block(8), None);
        assert_eq!(patch.adjacent_block(6, true), Some(10));
        assert_eq!(patch.adjacent_block(15, false), Some(10));
        assert_eq!(patch.adjacent_block(11, false), Some(6));
        assert_eq!(patch.adjacent_block(15, true), None);
        assert_eq!(patch.adjacent_change(7, true), Some(10));
        assert_eq!(patch.adjacent_change(6, false), None);
    }

    #[test]
    fn stages_one_added_line() {
        // The unselected `-two` and `-five` stay as context; `+FIVE` stays out.
        let expected = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,6 +1,7 @@ fn top()
 one
 two
+TWO
 three
 four
 five
 six
";
        assert_eq!(build(MODIFIED, 7..8, false), expected);
    }

    #[test]
    fn later_hunks_move_by_what_earlier_ones_changed() {
        let removed = build(MODIFIED, 6..7, false);
        assert!(removed.ends_with("@@ -1,6 +1,5 @@ fn top()\n one\n-two\n three\n four\n five\n six\n"), "{removed}");
        // `+TWO` adds a line (its `-two` stays), so the second hunk starts a line later.
        let out = build(MODIFIED, 7..16, false);
        assert!(out.contains("@@ -1,6 +1,7 @@ fn top()\n one\n two\n+TWO\n three\n four\n-five\n+FIVE\n six\n"), "{out}");
        assert!(out.ends_with("@@ -20,3 +21,4 @@\n twenty\n+new\n twenty-one\n twenty-two\n"), "{out}");
    }

    #[test]
    fn unstages_one_line_in_reverse() {
        // Reversed: the unselected `+TWO` stays as context, the unselected `-two` and
        // `-five` go.
        let out = build(MODIFIED, 11..12, true);
        assert!(out.ends_with("@@ -1,5 +1,6 @@ fn top()\n one\n TWO\n three\n four\n+FIVE\n six\n"), "{out}");
    }

    #[test]
    fn reverse_hunks_keep_their_new_side_and_move_the_old_side() {
        let all = build(MODIFIED, 6..16, true);
        assert!(all.contains("@@ -1,6 +1,6 @@ fn top()\n one\n-two\n+TWO\n"), "{all}");
        assert!(all.contains("@@ -20,3 +20,4 @@\n twenty\n+new\n"), "{all}");
        // Without `-two`, the first hunk's old side is a line shorter, and so the second
        // hunk's old side starts a line earlier.
        let out = build(MODIFIED, 7..16, true);
        assert!(out.contains("@@ -1,5 +1,6 @@ fn top()\n one\n+TWO\n three\n"), "{out}");
        assert!(out.contains("@@ -19,3 +20,4 @@\n"), "{out}");
        // The second hunk alone: nothing before it moves.
        let second = build(MODIFIED, 15..16, true);
        assert!(second.contains("@@ -20,3 +20,4 @@\n"), "{second}");
    }

    #[test]
    fn nothing_selected_is_no_patch() {
        let patch = Patch::parse(MODIFIED.as_bytes());
        assert_eq!(patch.build(8..10, false), Ok(None));
        assert_eq!(patch.build(0..5, true), Ok(None));
    }

    const NEW_FILE: &str = "\
diff --git a/my file.txt b/my file.txt
new file mode 100755
index 0000000..3333333
--- /dev/null
+++ b/my file.txt\t
@@ -0,0 +1,3 @@
+a
+b
+c
";

    #[test]
    fn stages_part_of_a_new_file_as_a_new_file() {
        let out = build(NEW_FILE, 7..8, false);
        assert_eq!(
            out,
            "diff --git a/my file.txt b/my file.txt\nnew file mode 100755\nindex 0000000..3333333\n--- /dev/null\n+++ b/my file.txt\t\n@@ -0,0 +1,1 @@\n+b\n"
        );
    }

    #[test]
    fn discarding_part_of_a_new_file_becomes_a_plain_change() {
        let out = build(NEW_FILE, 7..8, true);
        assert_eq!(
            out,
            "diff --git a/my file.txt b/my file.txt\n--- a/my file.txt\t\n+++ b/my file.txt\t\n@@ -1,2 +1,3 @@\n a\n+b\n c\n"
        );
        // All of it: still a new file, so reversing it removes the file.
        assert!(build(NEW_FILE, 6..9, true).contains("new file mode 100755\n"));
    }

    #[test]
    fn staging_part_of_a_deletion_becomes_a_plain_change() {
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
";
        let out = build(diff, 7..8, false);
        assert_eq!(out, "diff --git a/gone.txt b/gone.txt\n--- a/gone.txt\n+++ b/gone.txt\n@@ -1,3 +1,2 @@\n a\n-b\n c\n");
        assert!(build(diff, 0..9, false).contains("deleted file mode 100644\n"));
    }

    #[test]
    fn unstaging_part_of_a_rename_keeps_the_rename() {
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 80%
rename from old.txt
rename to new.txt
index 1111111..2222222 100644
--- a/old.txt
+++ b/new.txt
@@ -1,2 +1,2 @@
 keep
-x
+y
";
        let out = build(diff, 10..11, true);
        assert_eq!(out, "diff --git a/new.txt b/new.txt\n--- a/new.txt\n+++ b/new.txt\n@@ -1,1 +1,2 @@\n keep\n+y\n");
    }

    #[test]
    fn quoted_names_keep_their_quotes() {
        assert_eq!(with_side(b"\"b/tab\\tname\"", b'a'), b"\"a/tab\\tname\"");
        assert_eq!(with_side(b"b/x", b'a'), b"a/x");
        assert_eq!(with_side(b"/dev/null", b'a'), b"/dev/null");
    }

    const NO_NEWLINE: &str = "\
diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -1,2 +1,3 @@
 first
-last
\\ No newline at end of file
+last
+more
\\ No newline at end of file
";

    #[test]
    fn no_newline_markers_follow_their_line() {
        // The whole block: markers kept after their lines.
        let all = build(NO_NEWLINE, 6..11, false);
        assert!(all.ends_with("@@ -1,2 +1,3 @@\n first\n-last\n\\ No newline at end of file\n+last\n+more\n\\ No newline at end of file\n"), "{all}");
        // Just `-last`: `+last` and `+more` are dropped, and so is the marker after `+more`.
        let removed = build(NO_NEWLINE, 6..7, false);
        assert!(removed.ends_with("@@ -1,2 +1,1 @@\n first\n-last\n\\ No newline at end of file\n"), "{removed}");
    }

    #[test]
    fn a_line_after_a_last_line_without_newline_cant_be_split_off() {
        let patch = Patch::parse(NO_NEWLINE.as_bytes());
        assert_eq!(patch.build(9..10, false), Err(NoNewlineConflict));
    }

    #[test]
    fn keeps_carriage_returns_and_invalid_utf8() {
        let diff = b"diff --git a/w b/w\n--- a/w\n+++ b/w\n@@ -1,2 +1,2 @@\n a\r\n-b\xff\r\n+c\r\n";
        let patch = Patch::parse(diff);
        let out = patch.build(5..6, false).unwrap().unwrap();
        assert!(out.ends_with(b"@@ -1,2 +1,1 @@\n a\r\n-b\xff\r\n"), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn empty_context_lines_count_as_context() {
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let out = build(diff, 7..8, false);
        assert!(out.ends_with("@@ -1,3 +1,4 @@\n a\n \n b\n+c\n"), "{out}");
    }

    #[test]
    fn submodules_binaries_and_conflicts_have_nothing_to_stage() {
        let submodule = "diff --git a/sub b/sub\nindex 1111111..2222222 160000\n--- a/sub\n+++ b/sub\n@@ -1 +1 @@\n-Subproject commit 1111111\n+Subproject commit 2222222\n";
        let binary = "diff --git a/i.png b/i.png\nindex 1111111..2222222 100644\nBinary files a/i.png and b/i.png differ\n";
        let conflict = "diff --cc f\nindex 1111111,2222222..0000000\n--- a/f\n+++ b/f\n@@@ -1,1 -1,1 +1,5 @@@\n++<<<<<<< HEAD\n";
        for diff in [submodule, binary, conflict] {
            assert!(!Patch::parse(diff.as_bytes()).has_changes(), "{diff}");
        }
    }

    #[test]
    fn a_truncated_hunk_is_not_stageable() {
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n";
        assert!(!Patch::parse(diff.as_bytes()).has_changes());
    }
}
