//! Drawing: lazygit's framed panels, rows, main view, command log, bottom bar, popups.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::{ResizeHandleContext, ResizeHandleRenderer, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::{prelude::*, *};
use ubergit_core::{CmdKind, FileKind, Head, RepoSummary, Upstream};

use crate::batch::{BatchRow, Outcome, headline};
use crate::keymap::{self, *};
use crate::store::{RepoEntry, RepoStore};
use crate::text::{Line, age, truncate};
use crate::theme::{FONT, FONT_SIZE, LINE_HEIGHT, Palette};
use crate::workspace::{Dialog, MainContent, MenuItem, Panel, ScreenMode, TextKind, View, Workspace};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const GAP: Pixels = px(10.);
const HALF_GAP: Pixels = px(5.);
/// Padding on each side of a column boundary; the drag handle sits in the middle.
const COLUMN_GAP: Pixels = px(4.);
/// Smallest a resizable panel may get: one line of content in its frame.
const MIN_PANEL: Pixels = px(40.);
/// Advance of one Menlo glyph at 13px.
const CHAR_WIDTH: Pixels = px(7.83);

/// Resize handles are invisible until hovered or dragged, then show as a thin line in
/// the active border colour, so the gaps between frames look like lazygit's.
fn handle_appearance() -> ResizeHandleRenderer {
    Rc::new(|handle: &ResizeHandleContext, _: &mut Window, _: &mut App| {
        let line = div()
            .flex_none()
            .when(handle.is_active(), |d| d.bg(Palette::active_border()))
            .group_hover("handle", |style| style.bg(Palette::active_border()));
        let line = match handle.axis() {
            Axis::Horizontal => line.h_full().w(px(1.)),
            Axis::Vertical => line.w_full().h(px(1.)),
        };
        Some(line.into_any_element())
    })
}

fn frame_height(lines: usize) -> Pixels {
    LINE_HEIGHT * lines as f32 + px(10.)
}

impl Workspace {
    fn spinner(&self) -> &'static str {
        SPINNER[self.spinner % SPINNER.len()]
    }

    fn is_active(&self, panel: Panel) -> bool {
        self.focused == panel && self.dialog.is_none() && self.filter.is_none()
    }

    /// `[2]─Files - Worktrees - Submodules` with the current tab highlighted.
    fn title(&self, panel: Panel) -> Line {
        let active = self.is_active(panel);
        let mut line = Line::new();
        line.color(format!("[{}]", panel.jump_key()), Palette::dim());
        line.color("─", if active { Palette::active_border() } else { Palette::inactive_border() });
        let tabs = panel.tabs();
        let current = self.tab(panel);
        for (ix, tab) in tabs.iter().enumerate() {
            if ix > 0 {
                line.push(" - ");
            }
            if ix == current && tabs.len() > 1 || ix == current && active {
                if active {
                    line.bold(tab, Palette::active_border());
                } else {
                    line.color(tab, Palette::active_border());
                }
            } else {
                line.push(tab);
            }
        }
        line
    }

    /// A rounded, bordered box with its title inlaid in the top border. The title is a
    /// sibling drawn after the box, because gpui paints borders above an element's children.
    fn frame(&self, title: Line, active: bool, footer: Option<String>, body: impl IntoElement) -> Div {
        let border = if active { Palette::active_border() } else { Palette::inactive_border() };
        div()
            .relative()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(border)
                    .rounded(px(5.))
                    .pt(px(5.))
                    .pb(px(3.))
                    .px(px(3.))
                    .child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body)),
            )
            .child(
                // Spans the top border so a long title is clipped at the frame's edge
                // when the panel is resized narrow; only the title itself has a background.
                div().absolute().top(px(-10.)).left(px(8.)).right(px(8.)).flex().child(
                    div()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .px(px(3.))
                        .bg(Palette::bg())
                        .whitespace_nowrap()
                        .child(title.build()),
                ),
            )
            .when_some(footer, |d, footer| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(-10.))
                        .right(px(8.))
                        .px(px(3.))
                        .bg(Palette::bg())
                        .text_color(if active { Palette::active_border() } else { Palette::dim() })
                        .child(footer),
                )
            })
    }

    fn panel_frame(&self, panel: Panel, body: impl IntoElement, cx: &App) -> Div {
        let view = self.view_of(panel);
        let footer = view.is_list().then(|| {
            let store = self.store.read(cx);
            let len = self.visible(view, store).len();
            if len == 0 {
                "0 of 0".to_string()
            } else {
                format!("{} of {len}", self.cursor(view, len) + 1)
            }
        });
        let active = self.is_active(panel);
        let mut title = self.title(panel);
        if let Some(filter) = self.filters.get(&view).filter(|f| !f.is_empty()) {
            title.color(format!(" (filter: {filter})"), Palette::cyan());
        }
        if panel == Panel::Repos && !self.marked.is_empty() {
            title.color(format!(" ({} marked)", self.marked.len()), Palette::cyan());
        }
        self.frame(title, active, footer, body)
    }

    // ---- lists -----------------------------------------------------------------------------

    fn list_body(&self, panel: Panel, cx: &mut Context<Self>) -> AnyElement {
        let view = self.view_of(panel);
        if view == View::Status {
            let store = self.store.read(cx);
            return div()
                .px(px(4.))
                .whitespace_nowrap()
                .child(status_line(store).build())
                .into_any_element();
        }
        let store = self.store.read(cx);
        let count = self.visible(view, store).len();
        if count == 0 {
            let message = if view == View::Repos && store.scanning {
                "Scanning…"
            } else if view != View::Repos && store.selected.is_some() && store.selected_detail().is_none() {
                "Loading…"
            } else {
                ""
            };
            return div().px(px(4.)).text_color(Palette::dim()).child(message).into_any_element();
        }
        let scroll = self.lists.get(&view).map(|l| l.scroll.clone()).unwrap_or_default();
        uniform_list(
            view.context(),
            count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let store = this.store.read(cx);
                let visible = this.visible(view, store);
                let cursor = this.cursor(view, visible.len());
                let active = this.is_active(panel) || (this.focused == Panel::Main && this.last_side == panel);
                // Leave room for branch + status after the name in the Repos column.
                let repos_width = this
                    .layout
                    .columns
                    .read(cx)
                    .sizes()
                    .first()
                    .copied()
                    .filter(|w| *w > px(0.))
                    .unwrap_or(this.layout.repos_width);
                let panel_chars = (repos_width / CHAR_WIDTH) as usize;
                let reserved = if this.marked.is_empty() { 20 } else { 22 };
                let longest = store.repos.iter().map(|r| r.name().chars().count()).max().unwrap_or(0);
                let name_width = longest.min(panel_chars.saturating_sub(reserved).max(10));
                range
                    .filter_map(|pos| {
                        let ix = *visible.get(pos)?;
                        let line = this.row(view, ix, store, name_width);
                        let selected = pos == cursor;
                        Some(
                            div()
                                .id(pos)
                                .h(LINE_HEIGHT)
                                .px(px(4.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .when(selected, |d| {
                                    d.bg(if active { Palette::selection() } else { Palette::inactive_selection() })
                                })
                                .child(line.build())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        if view == View::Repos && event.modifiers.platform {
                                            this.toggle_mark_at(pos, cx);
                                        }
                                        this.click_row(view, pos, panel, window, cx)
                                    }),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&scroll)
        .size_full()
        .into_any_element()
    }

    fn row(&self, view: View, ix: usize, store: &RepoStore, name_width: usize) -> Line {
        if view == View::Repos {
            return self.repo_row(&store.repos[ix], name_width);
        }
        let Some(d) = store.selected_detail() else { return Line::new() };
        let mut line = Line::new();
        match view {
            View::Files => {
                let f = &d.files[ix];
                let (x, y) = match f.kind {
                    FileKind::Untracked => ('?', '?'),
                    _ => (f.index, f.worktree),
                };
                let show = |c: char| if c == '.' { ' ' } else { c };
                if f.kind == FileKind::Unmerged {
                    line.color(format!("{}{}", show(x), show(y)), Palette::red());
                } else {
                    line.color(show(x).to_string(), Palette::green());
                    line.color(show(y).to_string(), Palette::red());
                }
                line.push(" ");
                let color = if f.has_unstaged() { Palette::red() } else { Palette::green() };
                match &f.orig_path {
                    Some(orig) => line.color(format!("{orig} → {}", f.path), color),
                    None => line.color(&f.path, color),
                };
            }
            View::Worktrees => {
                let wt = &d.worktrees[ix];
                line.color(if wt.is_current { "* " } else { "  " }, Palette::green());
                line.push(wt.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                line.push(" ");
                line.color(wt.branch.as_deref().unwrap_or("(detached)"), Palette::blue());
            }
            View::Submodules => {
                let sm = &d.submodules[ix];
                let color = match sm.state {
                    ' ' => Palette::green(),
                    '-' => Palette::dim(),
                    _ => Palette::yellow(),
                };
                line.color(format!("{} ", sm.state), color);
                line.push(&sm.path);
                line.color(format!(" {}", sm.short_oid), Palette::dim());
            }
            View::Branches => {
                let b = &d.branches[ix];
                line.color(format!("{:<4}", age(b.committed)), Palette::dim());
                if b.is_head {
                    line.bold(format!("* {}", b.name), Palette::green());
                } else {
                    line.push(format!("  {}", b.name));
                }
                upstream_spans(&mut line, &b.upstream);
                if let (Some((ahead, behind)), Some(base)) = (b.base, store.selected_entry().and_then(|e| e.summary.as_ref()).and_then(|s| s.default_branch.as_ref())) {
                    let base_name = base.short.rsplit('/').next().unwrap_or(&base.short);
                    if base_name != b.name && (ahead > 0 || behind > 0) {
                        line.color(format!(" {base_name}{}", arrows(ahead, behind)), Palette::magenta());
                    }
                }
                if b.worktree.is_some() {
                    line.color(" (worktree)", Palette::cyan());
                }
            }
            View::Remotes => {
                let rb = &d.remote_branches[ix];
                line.color(format!("{}/", rb.remote), Palette::dim());
                line.push(&rb.name);
            }
            View::Tags => {
                let t = &d.tags[ix];
                line.color(&t.name, Palette::yellow());
                line.color(format!(" {}", t.subject), Palette::dim());
            }
            View::Commits => {
                let c = &d.commits[ix];
                line.color(&c.short_oid, Palette::yellow());
                line.color(format!(" {} ", initials(&c.author)), author_color(&c.author));
                let refs = compact_refs(&c.refs, d.remotes.iter().map(|r| r.name.as_str()));
                if !refs.is_empty() {
                    line.bold(format!("{} ", truncate(&refs, 32)), Palette::cyan());
                }
                line.push(&c.subject);
            }
            View::Reflog => {
                let r = &d.reflog[ix];
                line.color(&r.short_oid, Palette::yellow());
                line.color(format!(" {:<4}", age(r.time)), Palette::dim());
                line.push(&r.subject);
            }
            View::Stash => {
                let s = &d.stashes[ix];
                line.color(format!("{:<4}", age(s.time)), Palette::dim());
                line.color(format!("{} ", s.index), Palette::dim());
                line.push(&s.subject);
            }
            View::Repos | View::Status | View::Main => {}
        }
        line
    }

    fn repo_row(&self, entry: &RepoEntry, name_width: usize) -> Line {
        let mut line = Line::new();
        let mut name_end = name_width + 3;
        if !self.marked.is_empty() {
            mark_column(&mut line, self.marked.contains(&entry.location.root));
            name_end += 2;
        }
        let (glyph, color) = if entry.busy.is_some() {
            (self.spinner(), Palette::cyan())
        } else if entry.error.is_some() {
            ("⚠", Palette::red())
        } else if let Some(s) = &entry.summary {
            health(s, entry.fetch_error.is_some())
        } else {
            ("·", Palette::dim())
        };
        line.color(glyph, color);
        line.push(" ");
        line.push(truncate(entry.name(), name_width));
        line.pad_to(name_end);
        let Some(s) = &entry.summary else {
            if let Some(err) = &entry.error {
                line.color(truncate(err.lines().next().unwrap_or(""), 60), Palette::red());
            }
            return line;
        };
        match &s.head {
            Head::Branch(b) | Head::Unborn(b) => line.color(truncate(b, 22), Palette::blue()),
            Head::Detached(oid) => line.color(format!("@{}", oid.get(..7).unwrap_or(oid)), Palette::yellow()),
        };
        upstream_spans(&mut line, &s.upstream);
        if let Some(base) = &s.base {
            let base_name = base.base.rsplit('/').next().unwrap_or(&base.base);
            let on_base = s.head.branch_name() == Some(base_name)
                && matches!(&s.upstream, Upstream::Tracking { name, .. } if *name == base.base);
            if !on_base && (base.ahead > 0 || base.behind > 0) {
                line.color(format!(" {base_name}{}", arrows(base.ahead, base.behind)), Palette::magenta());
            }
        }
        if s.changes.files > 0 {
            let color = if s.changes.conflicted > 0 { Palette::red() } else { Palette::yellow() };
            line.color(format!(" *{}", s.changes.files), color);
        }
        if let Some(op) = &s.op {
            line.color(format!(" ({})", op.label()), Palette::yellow());
        }
        if entry.fetch_error.is_some() {
            line.color(" fetch failed", Palette::red());
        }
        line
    }

    // ---- layout ------------------------------------------------------------------------------

    /// A side panel's frame, filling whatever space its container gives it.
    fn side_panel(&self, panel: Panel, cx: &mut Context<Self>) -> Div {
        let body = self.list_body(panel, cx);
        self.panel_frame(panel, body, cx).size_full()
    }

    /// Status (fixed, one line) above Files / Branches / Commits / Stash, which share
    /// the rest and are resized by dragging the gaps between them.
    fn side_column(&self, cx: &mut Context<Self>) -> Div {
        let panels = [Panel::Files, Panel::Branches, Panel::Commits, Panel::Stash];
        let last = panels.len() - 1;
        let mut items = Vec::new();
        for (ix, panel) in panels.into_iter().enumerate() {
            let item = resizable_panel()
                .size_range(MIN_PANEL..Pixels::MAX)
                .when(ix > 0, |p| p.pt(HALF_GAP))
                .when(ix < last, |p| p.pb(HALF_GAP))
                .child(self.side_panel(panel, cx));
            // lazygit keeps Stash short; the others split the remaining height.
            items.push(if panel == Panel::Stash { item.size(frame_height(3) + HALF_GAP) } else { item });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.side_panel(Panel::Status, cx).h(frame_height(1)).flex_none())
            .child(
                div().flex_1().min_h(px(0.)).pt(GAP).child(
                    v_resizable("side-panels")
                        .with_state(&self.layout.side)
                        .with_handle_appearance(handle_appearance())
                        .children(items),
                ),
            )
    }

    /// Main view above the command log, split by a draggable gap.
    fn main_column(&self, cx: &mut Context<Self>) -> Div {
        let main = self.main_panel(cx);
        if !self.show_command_log || self.screen_mode != ScreenMode::Normal {
            return div().size_full().child(main);
        }
        div().size_full().child(
            v_resizable("main-split")
                .with_state(&self.layout.main)
                .with_handle_appearance(handle_appearance())
                .child(resizable_panel().size_range(MIN_PANEL..Pixels::MAX).pb(HALF_GAP).child(main))
                .child(
                    resizable_panel()
                        .size(frame_height(8) + HALF_GAP)
                        .size_range(MIN_PANEL..Pixels::MAX)
                        .pt(HALF_GAP)
                        .child(self.command_log(cx)),
                ),
        )
    }

    fn main_panel(&self, cx: &mut Context<Self>) -> Div {
        let active = self.is_active(Panel::Main);
        match &self.main.content {
            MainContent::Split { unstaged, staged } => {
                let unstaged_title = main_title("Unstaged changes", true);
                let staged_title = main_title("Staged changes", false);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(GAP)
                    .child(
                        self.frame(unstaged_title, active, None, text_list("unstaged", unstaged.clone(), TextKind::Diff, &self.main.scroll))
                            .flex_1()
                            .min_h(px(0.)),
                    )
                    .child(
                        self.frame(staged_title, false, None, text_list("staged", staged.clone(), TextKind::Diff, &self.main.scroll2))
                            .flex_1()
                            .min_h(px(0.)),
                    )
            }
            content => {
                let body = match content {
                    MainContent::Overview => self.overview(cx),
                    MainContent::Status => self.status_view(cx),
                    MainContent::Text { lines, kind } => text_list("main", lines.clone(), *kind, &self.main.scroll),
                    MainContent::Split { .. } => unreachable!(),
                };
                let title = main_title(&self.main.title, active);
                self.frame(title, active, None, body).size_full()
            }
        }
    }

    /// All repos at a glance: the dashboard shown while the Repos panel is focused.
    fn overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let visible = self.visible(View::Repos, store);
        let name_w = visible
            .iter()
            .map(|&ix| store.repos[ix].name().chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 32);
        let count = visible.len() + 1;
        uniform_list(
            "overview",
            count,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let store = this.store.read(cx);
                let visible = this.visible(View::Repos, store);
                let cursor = this.cursor(View::Repos, visible.len());
                let marks = !this.marked.is_empty();
                range
                    .map(|pos| {
                        let mut line = Line::new();
                        if pos == 0 {
                            if marks {
                                line.push("  ");
                            }
                            line.append(overview_header(name_w));
                        } else if let Some(&ix) = visible.get(pos - 1) {
                            let entry = &store.repos[ix];
                            if marks {
                                mark_column(&mut line, this.marked.contains(&entry.location.root));
                            }
                            line.append(overview_row(entry, name_w));
                        }
                        div()
                            .id(pos)
                            .h(LINE_HEIGHT)
                            .px(px(4.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .when(pos > 0 && pos - 1 == cursor, |d| d.bg(Palette::inactive_selection()))
                            .child(line.build())
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.main.scroll)
        .size_full()
        .into_any_element()
    }

    fn status_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let Some(entry) = store.selected_entry() else {
            return div().into_any_element();
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut kv = |key: &str, value: Line| {
            let mut line = Line::new();
            line.color(format!("{key:<14}"), Palette::dim());
            line.append(value);
            lines.push(line);
        };
        kv("Repository", Line::plain(entry.name()));
        kv("Path", Line::plain(entry.location.root.display().to_string()));
        if entry.location.is_linked_worktree() {
            kv("Worktree of", Line::plain(entry.location.common_dir.display().to_string()));
        }
        if let Some(err) = &entry.error {
            let mut l = Line::new();
            l.color(err, Palette::red());
            kv("Error", l);
        }
        if let Some(s) = &entry.summary {
            let mut head = Line::new();
            match &s.head {
                Head::Branch(b) => head.color(b, Palette::blue()),
                Head::Unborn(b) => head.color(format!("{b} (no commits yet)"), Palette::blue()),
                Head::Detached(oid) => head.color(format!("detached at {}", &oid[..oid.len().min(10)]), Palette::yellow()),
            };
            if let Some(op) = &s.op {
                head.color(format!("  ({})", op.label()), Palette::yellow());
            }
            kv("HEAD", head);
            let mut up = Line::new();
            match &s.upstream {
                Upstream::None => {
                    up.color("none", Palette::dim());
                }
                Upstream::Gone { name } => {
                    up.color(format!("{name} (upstream gone)"), Palette::red());
                }
                Upstream::Tracking { name, ahead, behind } => {
                    up.push(name);
                    upstream_spans(&mut up, &s.upstream);
                    if *ahead > 0 {
                        up.color(format!("  {ahead} to push"), Palette::dim());
                    }
                    if *behind > 0 {
                        up.color(format!("  {behind} to pull"), Palette::dim());
                    }
                }
            }
            kv("Upstream", up);
            let mut base = Line::new();
            match &s.base {
                Some(b) => {
                    base.push(&b.base);
                    if b.ahead == 0 && b.behind == 0 {
                        base.color(" ✓ up to date", Palette::green());
                    } else {
                        base.color(format!(" {} ahead, {} behind", b.ahead, b.behind), Palette::magenta());
                    }
                }
                None => {
                    base.color("unknown", Palette::dim());
                }
            }
            kv("Default branch", base);
            let c = s.changes;
            let mut changes = Line::new();
            if c.files == 0 {
                changes.color("clean", Palette::green());
            } else {
                changes.color(format!("{} staged", c.staged), Palette::green());
                changes.push(", ");
                changes.color(format!("{} unstaged", c.unstaged), Palette::red());
                changes.push(format!(", {} untracked", c.untracked));
                if c.conflicted > 0 {
                    changes.color(format!(", {} conflicted", c.conflicted), Palette::red());
                }
            }
            kv("Changes", changes);
            kv("Stashes", Line::plain(s.stash_count.to_string()));
            let mut fetch = Line::new();
            match s.last_fetch {
                Some(t) => fetch.push(format!("{} ago", age(Some(t)))),
                None => fetch.color("never", Palette::dim()),
            };
            if let Some(err) = &entry.fetch_error {
                fetch.color(format!("  failed: {err}"), Palette::red());
            }
            kv("Last fetch", fetch);
            if s.shallow {
                let mut l = Line::new();
                l.color("shallow clone: counts may be incomplete", Palette::yellow());
                kv("Note", l);
            }
        }
        if let Some(busy) = &entry.busy {
            let mut l = Line::new();
            l.color(format!("{} {busy}…", self.spinner()), Palette::cyan());
            kv("Running", l);
        }
        if let Some(d) = store.selected_detail() {
            for remote in &d.remotes {
                let mut l = Line::new();
                l.push(format!("{:<8} ", remote.name));
                l.color(&remote.url, Palette::dim());
                kv("Remote", l);
            }
        }
        div()
            .p(px(4.))
            .flex()
            .flex_col()
            .children(lines.into_iter().map(|l| div().h(LINE_HEIGHT).whitespace_nowrap().child(l.build())))
            .into_any_element()
    }

    fn command_log(&self, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        // Enough for a tall log; the frame clips from the top so the newest stay visible.
        let max = 80;
        let lines: Vec<Line> = store
            .log
            .iter()
            .rev()
            .take(max)
            .rev()
            .map(|record| {
                let mut line = Line::new();
                let name = store
                    .repos
                    .iter()
                    .find(|r| record.cwd.starts_with(&r.location.root))
                    .map(|r| r.name().to_string())
                    .unwrap_or_else(|| record.cwd.display().to_string());
                line.color(format!("{name}: "), Palette::cyan());
                let color = if record.kind == CmdKind::Network { Palette::blue() } else { Palette::fg() };
                line.color(&record.command, color);
                if let Some(err) = &record.error {
                    line.color(format!("  ✗ {err}"), Palette::red());
                } else if record.duration.as_millis() >= 500 {
                    line.color(format!("  ({:.1}s)", record.duration.as_secs_f32()), Palette::dim());
                }
                line
            })
            .collect();
        let mut title = Line::new();
        title.push("Command log");
        let body = div()
            .px(px(4.))
            .flex()
            .flex_col()
            .justify_end()
            .h_full()
            .children(lines.into_iter().map(|l| {
                div()
                    .h(LINE_HEIGHT)
                    .flex_none()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(l.build())
            }));
        self.frame(title, false, None, body).size_full()
    }

    fn bottom_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let _ = window;
        let store = self.store.read(cx);
        let bar = div().h(LINE_HEIGHT).px(px(8.)).flex().flex_row().gap(px(12.)).whitespace_nowrap().overflow_hidden();
        if let Some(filter) = &self.filter {
            return bar.child(
                div()
                    .key_context("Dialog")
                    .flex()
                    .flex_row()
                    .flex_1()
                    .gap(px(6.))
                    .items_center()
                    .child(div().text_color(Palette::cyan()).child("Filter:"))
                    .child(div().flex_1().child(Input::new(&filter.input).appearance(false))),
            );
        }
        let mut status = Line::new();
        if store.scanning {
            status.color(format!("Scanning {} {}", store.workdir.display(), self.spinner()), Palette::cyan());
        } else if let Some((done, total)) = store.fetch_round {
            status.color(format!("Fetching {done}/{total} {}", self.spinner()), Palette::cyan());
        } else if let Some(busy) = store.selected_entry().and_then(|e| e.busy.clone()) {
            status.color(format!("{busy} {}", self.spinner()), Palette::cyan());
        }
        let options = hints(self.current_view());
        let dirty = store
            .repos
            .iter()
            .filter(|r| r.summary.as_ref().is_some_and(|s| !s.changes.is_clean()))
            .count();
        let behind = store
            .repos
            .iter()
            .filter(|r| matches!(r.summary.as_ref().map(|s| &s.upstream), Some(Upstream::Tracking { behind, .. }) if *behind > 0))
            .count();
        let workdir = match dirs_home().and_then(|home| store.workdir.strip_prefix(home).ok().map(|p| p.to_path_buf())) {
            Some(rel) => format!("~/{}", rel.display()),
            None => store.workdir.display().to_string(),
        };
        let info = format!(
            "{} repos · {dirty} dirty · {behind} behind · {}",
            store.repos.len(),
            truncate_left(&workdir, 40)
        );
        bar.child(status.build())
            .child(div().flex_1().overflow_hidden().text_color(Palette::blue()).child(options))
            .child(div().text_color(Palette::green()).child(info))
    }

    fn dialog_layer(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let dialog = self.dialog.as_ref()?;
        let (title, body, width): (String, AnyElement, Pixels) = match dialog {
            Dialog::Confirm { title, message, .. } | Dialog::Error { title, message } => {
                let is_error = matches!(dialog, Dialog::Error { .. });
                let footer = if is_error { "<esc>/<enter>: close" } else { "<enter>: confirm · <esc>: cancel" };
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .children(message.lines().map(|l| {
                        div()
                            .text_color(if is_error { Palette::red() } else { Palette::fg() })
                            .child(if l.is_empty() { " ".to_string() } else { l.to_string() })
                    }))
                    .child(div().text_color(Palette::blue()).child(footer))
                    .into_any_element();
                (title.to_string(), body, px(620.))
            }
            Dialog::Prompt { title, input, .. } => {
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(Input::new(input))
                    .child(div().text_color(Palette::blue()).child("<enter>: confirm · <esc>: cancel"))
                    .into_any_element();
                (title.to_string(), body, px(520.))
            }
            Dialog::Commit { input, amend, .. } => {
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(Textarea::new(input).h(px(140.)))
                    .child(
                        div()
                            .text_color(Palette::blue())
                            .child("<enter>: commit · <shift-enter>: new line · <esc>: cancel"),
                    )
                    .into_any_element();
                let title = if *amend { "Amend last commit" } else { "Commit summary" };
                (title.to_string(), body, px(640.))
            }
            Dialog::Help { scroll, .. } => (
                format!("Keybindings ({})", self.current_view().context()),
                self.help_body(scroll, cx),
                px(640.),
            ),
            Dialog::Menu { title, items, selected } => (title.to_string(), self.menu_body(items, *selected, cx), px(640.)),
            Dialog::Results { title, rows, scroll, .. } => {
                // As many rows as fit in the popup (80% of the window) beside its title,
                // tally and hint lines; the rest scroll.
                let max_rows = ((window.viewport_size().height * 0.8 - px(110.)) / LINE_HEIGHT).max(3.) as usize;
                (title.to_string(), self.results_body(rows, scroll, max_rows), px(760.))
            }
        };
        let mut title_line = Line::new();
        title_line.bold(title, Palette::active_border());
        let inputs = matches!(dialog, Dialog::Prompt { .. } | Dialog::Commit { .. });
        let mut bordered = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(Palette::active_border())
            .rounded(px(5.))
            .bg(Palette::bg())
            .p(px(12.));
        if inputs {
            bordered = bordered.child(body);
        } else {
            let context = if matches!(dialog, Dialog::Help { .. }) { "Help" } else { "Popup" };
            bordered = bordered.child(
                div()
                    .key_context(context)
                    .track_focus(&self.dialog_focus)
                    .when(matches!(dialog, Dialog::Menu { .. }), |d| {
                        d.on_key_down(cx.listener(Self::menu_key_down))
                    })
                    .flex_1()
                    .min_h(px(0.))
                    .child(body),
            );
        }
        let boxed = div()
            .key_context("Dialog")
            .w(width)
            .max_h(relative(0.8))
            .relative()
            .child(bordered)
            .child(
                div()
                    .absolute()
                    .top(px(-10.))
                    .left(px(8.))
                    .px(px(3.))
                    .bg(Palette::bg())
                    .child(title_line.build()),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.35))
                .child(boxed),
        )
    }

    /// lazygit-style menu: each item's key, then its label; the selected one highlighted.
    fn menu_body(&self, items: &[MenuItem], selected: usize, cx: &mut Context<Self>) -> AnyElement {
        let rows = items.iter().enumerate().map(|(ix, item)| {
            let mut line = Line::new();
            line.color(format!("{:<3}", item.key), Palette::cyan());
            line.push(&item.label);
            div()
                .id(ix)
                .h(LINE_HEIGHT)
                .px(px(4.))
                .whitespace_nowrap()
                .overflow_hidden()
                .when(ix == selected, |d| d.bg(Palette::selection()))
                .child(line.build())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| this.pick_menu_item(ix, window, cx)),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().flex_col().children(rows))
            .child(div().text_color(Palette::blue()).child("<enter> or the item's key: select · <esc>: cancel"))
            .into_any_element()
    }

    /// One line per repo of a multi-repo action, then a tally.
    fn results_body(&self, rows: &[BatchRow], scroll: &UniformListScrollHandle, max_rows: usize) -> AnyElement {
        let name_w = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(4).clamp(4, 32);
        let (mut pending, mut done, mut skipped, mut failed) = (0, 0, 0, 0);
        let lines: Vec<Line> = rows
            .iter()
            .map(|row| {
                let mut line = Line::new();
                let (glyph, color) = match &row.outcome {
                    Outcome::Pending => (self.spinner(), Palette::cyan()),
                    Outcome::Done(_) => ("✓", Palette::green()),
                    Outcome::Skipped(_) => ("-", Palette::dim()),
                    Outcome::Failed(_) => ("✗", Palette::red()),
                };
                line.color(glyph, color);
                line.push(format!(" {:<name_w$}  ", truncate(&row.name, name_w)));
                match &row.outcome {
                    Outcome::Pending => pending += 1,
                    Outcome::Done(message) => {
                        done += 1;
                        line.push(message);
                    }
                    Outcome::Skipped(message) => {
                        skipped += 1;
                        line.color(message, Palette::dim());
                    }
                    Outcome::Failed(message) => {
                        failed += 1;
                        line.color(headline(message), Palette::red());
                    }
                }
                line
            })
            .collect();
        let mut tally = Line::new();
        if pending > 0 {
            tally.color(format!("{} of {} finished {}", rows.len() - pending, rows.len(), self.spinner()), Palette::cyan());
        } else {
            tally.color(format!("{done} done"), Palette::green());
            tally.color(format!(" · {skipped} skipped"), Palette::dim());
            tally.color(format!(" · {failed} failed"), if failed > 0 { Palette::red() } else { Palette::dim() });
        }
        if rows.len() > max_rows {
            tally.color(format!("  (j/k to scroll all {})", rows.len()), Palette::dim());
        }
        let height = LINE_HEIGHT * rows.len().clamp(1, max_rows) as f32;
        let lines = Arc::new(lines);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div().h(height).child(
                    uniform_list("results", lines.len(), move |range: Range<usize>, _, _| {
                        range
                            .map(|ix| div().h(LINE_HEIGHT).whitespace_nowrap().overflow_hidden().child(lines[ix].build()))
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(scroll)
                    .size_full(),
                ),
            )
            .child(tally.build())
            .child(div().text_color(Palette::blue()).child("<esc>/<enter>: close"))
            .into_any_element()
    }

    fn help_body(&self, scroll: &UniformListScrollHandle, _cx: &mut Context<Self>) -> AnyElement {
        let context = self.current_view().context();
        let mut lines: Vec<Line> = Vec::new();
        for (heading, ctx) in [(context, context), ("Global", "Panels")] {
            let entries: Vec<_> = keymap::HELP
                .iter()
                .filter(|e| e.context == ctx && !e.description.is_empty())
                .collect();
            if entries.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(Line::new());
            }
            let mut head = Line::new();
            head.bold(format!("--- {heading} ---"), Palette::fg());
            lines.push(head);
            for e in entries {
                let mut l = Line::new();
                l.color(format!("{:<12}", display_key(e.key)), Palette::cyan());
                l.push(e.description);
                lines.push(l);
            }
        }
        let lines = Arc::new(lines);
        div()
            .h(LINE_HEIGHT * 22.)
            .child(
                uniform_list("help", lines.len(), move |range: Range<usize>, _, _| {
                    range
                        .map(|ix| div().h(LINE_HEIGHT).whitespace_nowrap().child(lines[ix].build()))
                        .collect::<Vec<_>>()
                })
                .track_scroll(scroll)
                .size_full(),
            )
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.current_view();
        let columns = match self.screen_mode {
            // Repos | side panels | main, resized by dragging the gaps between columns.
            ScreenMode::Normal => div().size_full().child(
                h_resizable("columns")
                    .with_state(&self.layout.columns)
                    .with_handle_appearance(handle_appearance())
                    .child(
                        resizable_panel()
                            .size(self.layout.repos_width)
                            .size_range(px(160.)..Pixels::MAX)
                            .pr(COLUMN_GAP)
                            .child(self.side_panel(Panel::Repos, cx)),
                    )
                    .child(
                        resizable_panel()
                            .size(self.layout.side_width)
                            .size_range(px(200.)..Pixels::MAX)
                            .px(COLUMN_GAP)
                            .child(self.side_column(cx)),
                    )
                    .child(
                        resizable_panel()
                            .size_range(px(300.)..Pixels::MAX)
                            .pl(COLUMN_GAP)
                            .child(self.main_column(cx)),
                    ),
            ),
            ScreenMode::Half => {
                let side = self.main_source();
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .size_full()
                    .child(div().w(relative(0.4)).h_full().child(self.side_panel(side, cx)))
                    .child(div().flex_1().min_w(px(0.)).h_full().child(self.main_column(cx)))
            }
            ScreenMode::Full => {
                let content = if self.focused == Panel::Main {
                    self.main_column(cx)
                } else {
                    self.side_panel(self.focused, cx)
                };
                div().size_full().child(content)
            }
        };

        let bottom = self.bottom_bar(window, cx);
        let dialog = self.dialog_layer(window, cx);

        div()
            .key_context("Workspace")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(Palette::bg())
            .text_color(Palette::fg())
            .font_family(FONT)
            .text_size(FONT_SIZE)
            .line_height(LINE_HEIGHT)
            .on_action(cx.listener(Self::next_panel))
            .on_action(cx.listener(Self::prev_panel))
            .on_action(cx.listener(Self::focus_repos))
            .on_action(cx.listener(Self::focus_status))
            .on_action(cx.listener(Self::focus_files))
            .on_action(cx.listener(Self::focus_branches))
            .on_action(cx.listener(Self::focus_commits))
            .on_action(cx.listener(Self::focus_stash))
            .on_action(cx.listener(Self::focus_main))
            .on_action(cx.listener(Self::next_repo))
            .on_action(cx.listener(Self::prev_repo))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::scroll_main_down))
            .on_action(cx.listener(Self::scroll_main_up))
            .on_action(cx.listener(Self::half_page_main_down))
            .on_action(cx.listener(Self::half_page_main_up))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::start_filter))
            .on_action(cx.listener(Self::toggle_help))
            .on_action(cx.listener(Self::next_screen_mode))
            .on_action(cx.listener(Self::prev_screen_mode))
            .on_action(cx.listener(Self::toggle_command_log))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::fetch))
            .on_action(cx.listener(Self::fetch_all))
            .on_action(cx.listener(Self::pull))
            .on_action(cx.listener(Self::push))
            .on_action(cx.listener(Self::fast_forward_all))
            .on_action(cx.listener(Self::open_in_lazygit))
            .on_action(cx.listener(Self::toggle_mark))
            .on_action(cx.listener(Self::toggle_mark_all))
            .on_action(cx.listener(Self::checkout_by_name))
            .on_action(cx.listener(Self::new_branch_in_repos))
            .on_action(cx.listener(Self::switch_to_default))
            .on_action(cx.listener(Self::toggle_stage))
            .on_action(cx.listener(Self::toggle_stage_all))
            .on_action(cx.listener(Self::commit))
            .on_action(cx.listener(Self::amend))
            .on_action(cx.listener(Self::discard))
            .on_action(cx.listener(Self::stash_all))
            .on_action(cx.listener(Self::stash_options))
            .on_action(cx.listener(Self::checkout))
            .on_action(cx.listener(Self::checkout_previous))
            .on_action(cx.listener(Self::new_branch))
            .on_action(cx.listener(Self::delete_branch))
            .on_action(cx.listener(Self::fast_forward))
            .on_action(cx.listener(Self::set_upstream))
            .on_action(cx.listener(Self::stash_apply))
            .on_action(cx.listener(Self::stash_pop))
            .on_action(cx.listener(Self::stash_drop))
            .on_action(cx.listener(Self::rename_stash))
            .on_action(cx.listener(Self::branch_from_stash))
            .on_action(cx.listener(Self::confirm_dialog))
            .on_action(cx.listener(Self::close_dialog))
            .child(
                div().key_context("Panels").flex_1().min_h(px(0.)).child(
                    div()
                        .key_context(view.context())
                        .track_focus(&self.focus)
                        .size_full()
                        .pt(px(14.))
                        .pb(px(12.))
                        .px(px(8.))
                        .child(columns),
                ),
            )
            .child(bottom)
            .children(dialog)
    }
}

// ---- row helpers -----------------------------------------------------------------------------

fn arrows(ahead: u32, behind: u32) -> String {
    let mut s = String::new();
    if behind > 0 {
        s.push_str(&format!("↓{behind}"));
    }
    if ahead > 0 {
        s.push_str(&format!("↑{ahead}"));
    }
    s
}

/// lazygit's upstream marker: `✓`, `↓2↑1` (yellow), `gone` (red), nothing without upstream.
fn upstream_spans(line: &mut Line, upstream: &Upstream) {
    match upstream {
        Upstream::None => {}
        Upstream::Gone { .. } => {
            line.color(" gone", Palette::red());
        }
        Upstream::Tracking { ahead: 0, behind: 0, .. } => {
            line.color(" ✓", Palette::green());
        }
        Upstream::Tracking { ahead, behind, .. } => {
            line.color(format!(" {}", arrows(*ahead, *behind)), Palette::yellow());
        }
    }
}

/// Row glyph: green clean & synced, yellow needs attention, red conflicts/failures.
fn health(s: &RepoSummary, fetch_failed: bool) -> (&'static str, Hsla) {
    if s.changes.conflicted > 0 || fetch_failed || matches!(s.upstream, Upstream::Gone { .. }) {
        ("●", Palette::red())
    } else if s.is_settled() && s.base.as_ref().is_none_or(|b| b.behind == 0) {
        ("●", Palette::green())
    } else if !s.changes.is_clean() || s.op.is_some() {
        ("●", Palette::yellow())
    } else if matches!(s.upstream, Upstream::Tracking { behind, .. } if behind > 0) {
        ("●", Palette::blue())
    } else {
        ("○", Palette::dim())
    }
}

fn status_line(store: &RepoStore) -> Line {
    let mut line = Line::new();
    let Some(entry) = store.selected_entry() else {
        line.color("no repository selected", Palette::dim());
        return line;
    };
    if let Some(s) = &entry.summary {
        match &s.upstream {
            Upstream::Tracking { ahead: 0, behind: 0, .. } => {
                line.color("✓ ", Palette::green());
            }
            Upstream::Tracking { ahead, behind, .. } => {
                line.color(format!("{} ", arrows(*ahead, *behind)), Palette::yellow());
            }
            Upstream::Gone { .. } => {
                line.color("(upstream gone) ", Palette::red());
            }
            Upstream::None => {}
        }
        if let Some(op) = &s.op {
            line.color(format!("({}) ", op.label()), Palette::yellow());
        }
        line.push(entry.name());
        line.push(" → ");
        match &s.head {
            Head::Branch(b) | Head::Unborn(b) => line.color(b, Palette::blue()),
            Head::Detached(oid) => line.color(format!("@{}", oid.get(..7).unwrap_or(oid)), Palette::yellow()),
        };
    } else {
        line.push(entry.name());
    }
    line
}

/// `✓ ` before a marked repo, blank before the others, while any repo is marked.
fn mark_column(line: &mut Line, marked: bool) {
    if marked {
        line.bold("✓ ", Palette::cyan());
    } else {
        line.push("  ");
    }
}

fn overview_header(name_w: usize) -> Line {
    let mut line = Line::new();
    let header = format!(
        "  {:<name_w$}  {:<22} {:<9} {:<16} {:<16} {:>5}  {:<7} {}",
        "REPO", "BRANCH", "UPSTREAM", "VS DEFAULT", "CHANGES", "STASH", "FETCHED", "STATE"
    );
    line.bold(header, Palette::dim());
    line
}

fn overview_row(entry: &RepoEntry, name_w: usize) -> Line {
    let mut line = Line::new();
    let (glyph, color) = match &entry.summary {
        _ if entry.error.is_some() => ("⚠", Palette::red()),
        Some(s) => health(s, entry.fetch_error.is_some()),
        None => ("·", Palette::dim()),
    };
    line.color(glyph, color);
    line.push(" ");
    line.push(format!("{:<name_w$}  ", truncate(entry.name(), name_w)));
    let Some(s) = &entry.summary else {
        if let Some(err) = &entry.error {
            line.color(truncate(err.lines().next().unwrap_or(""), 80), Palette::red());
        }
        return line;
    };
    let branch = match &s.head {
        Head::Branch(b) | Head::Unborn(b) => truncate(b, 22),
        Head::Detached(oid) => format!("@{}", oid.get(..7).unwrap_or(oid)),
    };
    line.color(format!("{branch:<22} "), if matches!(s.head, Head::Detached(_)) { Palette::yellow() } else { Palette::blue() });
    let (upstream, up_color) = match &s.upstream {
        Upstream::None => ("-".to_string(), Palette::dim()),
        Upstream::Gone { .. } => ("gone".to_string(), Palette::red()),
        Upstream::Tracking { ahead: 0, behind: 0, .. } => ("✓".to_string(), Palette::green()),
        Upstream::Tracking { ahead, behind, .. } => (arrows(*ahead, *behind), Palette::yellow()),
    };
    line.color(format!("{upstream:<9} "), up_color);
    let (base, base_color) = match &s.base {
        None => ("-".to_string(), Palette::dim()),
        Some(b) if b.ahead == 0 && b.behind == 0 => (format!("{} ✓", short_base(&b.base)), Palette::green()),
        Some(b) => (format!("{} {}", short_base(&b.base), arrows(b.ahead, b.behind)), Palette::magenta()),
    };
    line.color(format!("{:<16} ", truncate(&base, 16)), base_color);
    let c = s.changes;
    let changes = if c.files == 0 {
        "clean".to_string()
    } else {
        let mut parts = Vec::new();
        if c.staged > 0 {
            parts.push(format!("+{}", c.staged));
        }
        if c.unstaged > 0 {
            parts.push(format!("~{}", c.unstaged));
        }
        if c.untracked > 0 {
            parts.push(format!("?{}", c.untracked));
        }
        if c.conflicted > 0 {
            parts.push(format!("!{}", c.conflicted));
        }
        parts.join(" ")
    };
    let changes_color = if c.conflicted > 0 {
        Palette::red()
    } else if c.files > 0 {
        Palette::yellow()
    } else {
        Palette::dim()
    };
    line.color(format!("{changes:<16} "), changes_color);
    line.color(format!("{:>5}  ", if s.stash_count > 0 { s.stash_count.to_string() } else { "-".into() }), Palette::dim());
    let fetched = if s.last_fetch.is_some() { age(s.last_fetch) } else { "never".into() };
    line.color(format!("{fetched:<7} "), if entry.fetch_error.is_some() { Palette::red() } else { Palette::dim() });
    if let Some(busy) = &entry.busy {
        line.color(format!("{busy}…"), Palette::cyan());
    } else if let Some(op) = &s.op {
        line.color(op.label(), Palette::yellow());
    } else if let Some(err) = &entry.fetch_error {
        line.color(format!("fetch failed: {}", truncate(err, 60)), Palette::red());
    } else if s.shallow {
        line.color("shallow", Palette::dim());
    }
    line
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Keeps the end of `s`, e.g. `…/services/billing`.
fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(count - max + 1).collect::<String>())
    }
}

fn short_base(base: &str) -> &str {
    base.rsplit('/').next().unwrap_or(base)
}

fn main_title(title: &str, active: bool) -> Line {
    let mut line = Line::new();
    line.color("[0]", Palette::dim());
    line.color("─", if active { Palette::active_border() } else { Palette::inactive_border() });
    if active {
        line.bold(title, Palette::active_border());
    } else {
        line.push(title);
    }
    line
}

fn text_list(id: &'static str, lines: Arc<Vec<String>>, kind: TextKind, scroll: &UniformListScrollHandle) -> AnyElement {
    uniform_list(id, lines.len(), move |range: Range<usize>, _, _| {
        range
            .map(|ix| {
                let text = &lines[ix];
                let line = match kind {
                    TextKind::Diff => diff_line(text),
                    TextKind::Log => log_line(text),
                    TextKind::Plain => Line::plain(text),
                };
                div().h(LINE_HEIGHT).px(px(4.)).whitespace_nowrap().child(line.build())
            })
            .collect::<Vec<_>>()
    })
    .track_scroll(scroll)
    .size_full()
    .into_any_element()
}

fn diff_line(s: &str) -> Line {
    let mut line = Line::new();
    if s.starts_with("+++") || s.starts_with("---") || s.starts_with("diff --git") {
        line.bold(s, Palette::fg());
    } else if s.starts_with('+') {
        line.color(s, Palette::green());
    } else if s.starts_with('-') {
        line.color(s, Palette::red());
    } else if s.starts_with("@@") {
        line.color(s, Palette::cyan());
    } else if s.starts_with("commit ") || s.starts_with("tag ") {
        line.color(s, Palette::yellow());
    } else if ["index ", "new file", "deleted file", "similarity", "rename ", "old mode", "new mode", "Binary files"]
        .iter()
        .any(|p| s.starts_with(p))
    {
        line.color(s, Palette::dim());
    } else if let Some((key, rest)) = s.split_once(':')
        && ["Author", "AuthorDate", "Commit", "CommitDate", "Merge", "Tagger", "Date"].contains(&key)
    {
        line.color(format!("{key}:"), Palette::dim());
        line.push(rest);
    } else if let Some((file, stat)) = s.split_once(" | ") {
        line.push(file);
        line.push(" | ");
        for ch in stat.chars() {
            match ch {
                '+' => line.color("+", Palette::green()),
                '-' => line.color("-", Palette::red()),
                c => line.push(c.to_string()),
            };
        }
    } else {
        line.push(s);
    }
    line
}

/// `git log --graph --format='%h %ad %an%d%n%s'` lines.
fn log_line(s: &str) -> Line {
    let mut line = Line::new();
    let split = s.find(|c: char| c.is_ascii_alphanumeric()).unwrap_or(s.len());
    let (graph, rest) = s.split_at(split);
    line.color(graph, Palette::magenta());
    let hash_len = rest.find(' ').unwrap_or(rest.len());
    let is_header = hash_len >= 7 && rest[..hash_len].chars().all(|c| c.is_ascii_hexdigit());
    if is_header {
        line.color(&rest[..hash_len], Palette::yellow());
        let tail = &rest[hash_len..];
        match tail.find(" (") {
            Some(ix) if tail.ends_with(')') => {
                line.color(&tail[..ix], Palette::dim());
                line.bold(&tail[ix..], Palette::cyan());
            }
            _ => {
                line.color(tail, Palette::dim());
            }
        }
    } else {
        line.push(rest);
    }
    line
}

/// `HEAD -> main, origin/main, tag: v1` → `v1` plus other local branch heads, like
/// lazygit's commit list (the checked-out branch and remote refs are left out).
fn compact_refs<'a>(refs: &str, remotes: impl Iterator<Item = &'a str>) -> String {
    let remotes: Vec<&str> = remotes.collect();
    refs.split(", ")
        .filter_map(|r| {
            if r.starts_with("HEAD") {
                None
            } else if let Some(tag) = r.strip_prefix("tag: ") {
                Some(tag.to_string())
            } else if remotes.iter().any(|remote| r.starts_with(&format!("{remote}/"))) {
                None
            } else {
                Some(r.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn initials(author: &str) -> String {
    let mut chars = author.split_whitespace().filter_map(|w| w.chars().next());
    let first = chars.next().unwrap_or(' ');
    let second = chars.next_back().or_else(|| author.chars().nth(1)).unwrap_or(' ');
    format!("{first}{second}").to_uppercase()
}

fn author_color(author: &str) -> Hsla {
    let hash = author.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let colors = [Palette::cyan(), Palette::magenta(), Palette::yellow(), Palette::blue(), Palette::green()];
    colors[hash as usize % colors.len()]
}

fn hints(view: View) -> String {
    let parts: &[(&str, &str)] = match view {
        View::Repos => &[
            ("Mark", "<space>"),
            ("Checkout", "c"),
            ("New branch", "n"),
            ("Default branch", "m"),
            ("Pull", "p"),
            ("Fetch all", "F"),
            ("Update", "U"),
        ],
        View::Files => &[("Stage", "<space>"), ("Stage all", "a"), ("Commit", "c"), ("Discard", "d"), ("Stash", "s")],
        View::Branches => &[("Checkout", "<space>"), ("New", "n"), ("Delete", "d"), ("Fast-forward", "f"), ("Upstream", "u")],
        View::Remotes => &[("Checkout", "<space>"), ("New branch", "n"), ("Fetch", "f")],
        View::Tags | View::Commits => &[("Checkout", "<space>"), ("View", "<enter>")],
        View::Stash => &[("Apply", "<space>"), ("Pop", "g"), ("Drop", "d"), ("Rename", "r"), ("Branch", "n")],
        View::Main => &[("Scroll", "j/k"), ("Back", "<esc>")],
        _ => &[("Push", "P"), ("Pull", "p"), ("Fetch", "f")],
    };
    let mut out: Vec<String> = parts.iter().map(|(what, key)| format!("{what}: {key}")).collect();
    out.push("Repo: {/}".into());
    out.push("Keybindings: ?".into());
    out.join(" | ")
}
