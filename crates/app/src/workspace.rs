//! The root view: lazygit's panels plus the Repos column, focus, selection and actions.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::ResizableState;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{prelude::*, *};
use ubergit_core::detail::{self, FileDiff, RepoDetail};
use ubergit_core::{FileKind, Git, GitError, GitOutput, Head, RepoLocation, Upstream, ops};

use crate::batch::BatchRow;
use crate::keymap::*;
use crate::store::RepoStore;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Panel {
    Repos,
    Status,
    Files,
    Branches,
    Commits,
    Stash,
    Main,
}

impl Panel {
    pub const SIDE: [Panel; 6] = [
        Panel::Repos,
        Panel::Status,
        Panel::Files,
        Panel::Branches,
        Panel::Commits,
        Panel::Stash,
    ];

    pub fn tabs(self) -> &'static [&'static str] {
        match self {
            Panel::Repos => &["Repos"],
            Panel::Status => &["Status"],
            Panel::Files => &["Files", "Worktrees", "Submodules"],
            Panel::Branches => &["Local branches", "Remotes", "Tags"],
            Panel::Commits => &["Commits", "Reflog"],
            Panel::Stash => &["Stash"],
            Panel::Main => &["Main"],
        }
    }

    pub fn jump_key(self) -> &'static str {
        match self {
            Panel::Repos => "⌘R",
            Panel::Status => "1",
            Panel::Files => "2",
            Panel::Branches => "3",
            Panel::Commits => "4",
            Panel::Stash => "5",
            Panel::Main => "0",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum View {
    Repos,
    Status,
    Files,
    Worktrees,
    Submodules,
    Branches,
    Remotes,
    Tags,
    Commits,
    Reflog,
    Stash,
    Main,
}

impl View {
    pub fn context(self) -> &'static str {
        match self {
            View::Repos => "Repos",
            View::Status => "Status",
            View::Files => "Files",
            View::Worktrees => "Worktrees",
            View::Submodules => "Submodules",
            View::Branches => "Branches",
            View::Remotes => "Remotes",
            View::Tags => "Tags",
            View::Commits => "Commits",
            View::Reflog => "Reflog",
            View::Stash => "Stash",
            View::Main => "Main",
        }
    }

    pub fn is_list(self) -> bool {
        !matches!(self, View::Status | View::Main)
    }

    /// Lists that belong to the selected repo (reset when switching repos).
    fn is_repo_detail(self) -> bool {
        self.is_list() && self != View::Repos
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenMode {
    Normal,
    Half,
    Full,
}

pub struct ListState {
    pub selected: usize,
    pub scroll: UniformListScrollHandle,
}

impl Default for ListState {
    fn default() -> Self {
        Self {
            selected: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MainKey {
    Overview,
    Status,
    Diff { root: PathBuf, path: String },
    Show { root: PathBuf, rev: String },
    Stash { root: PathBuf, index: usize },
    Log { root: PathBuf, rev: String },
    Message(String),
}

impl MainKey {
    /// Content that must be reloaded when the repo's detail changes.
    fn is_repo_state(&self) -> bool {
        matches!(self, MainKey::Diff { .. } | MainKey::Stash { .. } | MainKey::Log { .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextKind {
    Diff,
    Log,
    Plain,
}

#[derive(Clone)]
pub enum MainContent {
    Overview,
    Status,
    Text { lines: Arc<Vec<String>>, kind: TextKind },
    Split { unstaged: Arc<Vec<String>>, staged: Arc<Vec<String>> },
}

pub struct MainState {
    pub key: Option<MainKey>,
    generation: u64,
    pub title: SharedString,
    pub content: MainContent,
    pub top: usize,
    pub scroll: UniformListScrollHandle,
    pub scroll2: UniformListScrollHandle,
    task: Option<Task<()>>,
}

pub type ConfirmFn = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;
pub type SubmitFn = Box<dyn FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>;

pub enum Dialog {
    Confirm {
        title: SharedString,
        message: SharedString,
        on_confirm: Option<ConfirmFn>,
    },
    Prompt {
        title: SharedString,
        input: Entity<InputState>,
        on_submit: Option<SubmitFn>,
    },
    Commit {
        input: Entity<TextareaState>,
        amend: bool,
        root: PathBuf,
    },
    Error {
        title: SharedString,
        message: SharedString,
    },
    Help {
        scroll: UniformListScrollHandle,
        top: usize,
    },
    /// Per-repo outcomes of a multi-repo action, filled in as each repo finishes.
    Results {
        id: u64,
        title: SharedString,
        rows: Vec<BatchRow>,
        scroll: UniformListScrollHandle,
    },
}

pub struct Filter {
    pub view: View,
    pub input: Entity<InputState>,
    _subscription: Subscription,
}

/// Drag-resizable split state: the three columns, the side panels and main/command log.
pub struct Layout {
    pub columns: Entity<ResizableState>,
    pub side: Entity<ResizableState>,
    pub main: Entity<ResizableState>,
    /// Initial column widths, from the window size at startup.
    pub repos_width: Pixels,
    pub side_width: Pixels,
}

/// Default share of the window width for the Repos and side columns.
pub const REPOS_SHARE: f32 = 0.28;
pub const SIDE_SHARE: f32 = 0.26;

pub struct Workspace {
    pub store: Entity<RepoStore>,
    pub focus: FocusHandle,
    pub dialog_focus: FocusHandle,
    pub focused: Panel,
    pub last_side: Panel,
    tabs: HashMap<Panel, usize>,
    pub lists: HashMap<View, ListState>,
    pub filters: HashMap<View, String>,
    pub filter: Option<Filter>,
    pub screen_mode: ScreenMode,
    pub show_command_log: bool,
    pub main: MainState,
    pub dialog: Option<Dialog>,
    pub spinner: usize,
    pub layout: Layout,
    /// Repos marked in the Repos panel; actions started there run on all of them.
    pub marked: HashSet<PathBuf>,
    /// Identifies the multi-repo action whose results popup is showing.
    pub(crate) last_batch: u64,
    _subscriptions: Vec<Subscription>,
    _ticker: Task<()>,
}

pub const PAGE: isize = 10;

impl Workspace {
    pub fn new(store: Entity<RepoStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let observe = cx.observe(&store, |this, _, cx| this.on_store_changed(cx));
        // Spinner animation while anything is busy.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let alive = this.update(cx, |this, cx| {
                    let store = this.store.read(cx);
                    if store.scanning || store.fetch_round.is_some() || store.repos.iter().any(|r| r.busy.is_some()) {
                        this.spinner = this.spinner.wrapping_add(1);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            store,
            focus,
            dialog_focus: cx.focus_handle(),
            focused: Panel::Repos,
            last_side: Panel::Repos,
            tabs: HashMap::new(),
            lists: HashMap::new(),
            filters: HashMap::new(),
            filter: None,
            screen_mode: ScreenMode::Normal,
            show_command_log: true,
            main: MainState {
                key: None,
                generation: 0,
                title: "".into(),
                content: MainContent::Overview,
                top: 0,
                scroll: UniformListScrollHandle::new(),
                scroll2: UniformListScrollHandle::new(),
                task: None,
            },
            dialog: None,
            spinner: 0,
            layout: {
                let width = window.viewport_size().width;
                Layout {
                    columns: cx.new(|_| ResizableState::default()),
                    side: cx.new(|_| ResizableState::default()),
                    main: cx.new(|_| ResizableState::default()),
                    repos_width: width * REPOS_SHARE,
                    side_width: width * SIDE_SHARE,
                }
            },
            marked: HashSet::new(),
            last_batch: 0,
            _subscriptions: vec![observe],
            _ticker: ticker,
        }
    }

    // ---- views, lists and selection -------------------------------------------------

    pub fn tab(&self, panel: Panel) -> usize {
        self.tabs.get(&panel).copied().unwrap_or(0)
    }

    pub fn view_of(&self, panel: Panel) -> View {
        match (panel, self.tab(panel)) {
            (Panel::Repos, _) => View::Repos,
            (Panel::Status, _) => View::Status,
            (Panel::Files, 1) => View::Worktrees,
            (Panel::Files, 2) => View::Submodules,
            (Panel::Files, _) => View::Files,
            (Panel::Branches, 1) => View::Remotes,
            (Panel::Branches, 2) => View::Tags,
            (Panel::Branches, _) => View::Branches,
            (Panel::Commits, 1) => View::Reflog,
            (Panel::Commits, _) => View::Commits,
            (Panel::Stash, _) => View::Stash,
            (Panel::Main, _) => View::Main,
        }
    }

    pub fn current_view(&self) -> View {
        self.view_of(self.focused)
    }

    /// The side panel whose selection drives the main view.
    pub fn main_source(&self) -> Panel {
        if self.focused == Panel::Main { self.last_side } else { self.focused }
    }

    pub fn list(&mut self, view: View) -> &mut ListState {
        self.lists.entry(view).or_default()
    }

    pub fn item_count(view: View, store: &RepoStore) -> usize {
        let Some(detail) = store.selected_detail() else {
            return if view == View::Repos { store.repos.len() } else { 0 };
        };
        match view {
            View::Repos => store.repos.len(),
            View::Files => detail.files.len(),
            View::Worktrees => detail.worktrees.len(),
            View::Submodules => detail.submodules.len(),
            View::Branches => detail.branches.len(),
            View::Remotes => detail.remote_branches.len(),
            View::Tags => detail.tags.len(),
            View::Commits => detail.commits.len(),
            View::Reflog => detail.reflog.len(),
            View::Stash => detail.stashes.len(),
            View::Status | View::Main => 0,
        }
    }

    /// Text matched by `/` filtering.
    fn item_text(view: View, ix: usize, store: &RepoStore) -> String {
        if view == View::Repos {
            let entry = &store.repos[ix];
            let branch = entry
                .summary
                .as_ref()
                .and_then(|s| s.head.branch_name().map(str::to_string))
                .unwrap_or_default();
            return format!("{} {branch}", entry.name());
        }
        let Some(d) = store.selected_detail() else { return String::new() };
        match view {
            View::Files => d.files[ix].path.clone(),
            View::Worktrees => d.worktrees[ix].path.display().to_string(),
            View::Submodules => d.submodules[ix].path.clone(),
            View::Branches => d.branches[ix].name.clone(),
            View::Remotes => format!("{}/{}", d.remote_branches[ix].remote, d.remote_branches[ix].name),
            View::Tags => d.tags[ix].name.clone(),
            View::Commits => format!("{} {} {}", d.commits[ix].short_oid, d.commits[ix].subject, d.commits[ix].author),
            View::Reflog => d.reflog[ix].subject.clone(),
            View::Stash => d.stashes[ix].subject.clone(),
            _ => String::new(),
        }
    }

    /// Indices of the items shown in `view` after filtering.
    pub fn visible(&self, view: View, store: &RepoStore) -> Vec<usize> {
        let count = Self::item_count(view, store);
        match self.filters.get(&view).filter(|f| !f.is_empty()) {
            None => (0..count).collect(),
            Some(filter) => {
                let filter = filter.to_lowercase();
                (0..count)
                    .filter(|&ix| Self::item_text(view, ix, store).to_lowercase().contains(&filter))
                    .collect()
            }
        }
    }

    /// Position in the visible list (clamped).
    pub fn cursor(&self, view: View, visible_len: usize) -> usize {
        let selected = self.lists.get(&view).map_or(0, |l| l.selected);
        selected.min(visible_len.saturating_sub(1))
    }

    /// The underlying item index selected in `view`.
    pub fn selected_ix(&self, view: View, store: &RepoStore) -> Option<usize> {
        let visible = self.visible(view, store);
        visible.get(self.cursor(view, visible.len())).copied()
    }

    pub fn selected_root(&self, cx: &App) -> Option<PathBuf> {
        let store = self.store.read(cx);
        self.selected_ix(View::Repos, store)
            .map(|ix| store.repos[ix].location.root.clone())
    }

    fn move_cursor(&mut self, view: View, to: impl FnOnce(usize, usize) -> usize, cx: &mut Context<Self>) {
        let len = self.visible(view, self.store.read(cx)).len();
        if len == 0 {
            return;
        }
        let current = self.cursor(view, len);
        let next = to(current, len).min(len - 1);
        let list = self.list(view);
        list.selected = next;
        list.scroll.scroll_to_item(next, ScrollStrategy::Top);
        self.after_change(cx);
    }

    fn move_by(&mut self, view: View, delta: isize, cx: &mut Context<Self>) {
        self.move_cursor(
            view,
            |current, _| current.saturating_add_signed(delta),
            cx,
        );
    }

    pub fn click_row(&mut self, view: View, position: usize, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        self.list(view).selected = position;
        self.focus_panel(panel, window, cx);
    }

    /// Re-syncs everything derived from focus/selection after any change.
    pub fn after_change(&mut self, cx: &mut Context<Self>) {
        let root = self.selected_root(cx);
        if self.store.read(cx).selected != root {
            for (view, list) in self.lists.iter_mut() {
                if view.is_repo_detail() {
                    list.selected = 0;
                    list.scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
            }
            self.filters.retain(|view, _| !view.is_repo_detail());
            self.store.update(cx, |store, cx| store.select(root, cx));
        }
        self.sync_main(cx);
        cx.notify();
    }

    fn on_store_changed(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if !store.scanning {
            self.marked.retain(|root| store.index_of(root).is_some());
        }
        self.after_change(cx);
    }

    fn focus_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if panel != Panel::Main {
            self.last_side = panel;
        }
        self.focused = panel;
        window.focus(&self.focus, cx);
        self.after_change(cx);
    }

    // ---- main view ---------------------------------------------------------------------

    fn desired_main(&self, store: &RepoStore) -> (MainKey, SharedString) {
        let source = self.main_source();
        if source == Panel::Repos {
            return (MainKey::Overview, "Overview".into());
        }
        let Some(root) = store.selected.clone() else {
            return (MainKey::Message("No repository selected".into()), "".into());
        };
        if source == Panel::Status {
            return (MainKey::Status, "Status".into());
        }
        let Some(detail) = store.selected_detail() else {
            let message = match &store.detail {
                Some(d) if d.root == root && d.error.is_some() => d.error.clone().unwrap_or_default(),
                _ => "Loading…".into(),
            };
            return (MainKey::Message(message), "".into());
        };
        let view = self.view_of(source);
        let Some(ix) = self.selected_ix(view, store) else {
            let empty = match view {
                View::Files => "No changed files",
                View::Stash => "No stash entries",
                View::Worktrees => "No worktrees",
                View::Submodules => "No submodules",
                _ => "Nothing here",
            };
            return (MainKey::Message(empty.into()), "".into());
        };
        match view {
            View::Files => {
                let file = &detail.files[ix];
                let title = if file.has_staged() && file.has_unstaged() { "Unstaged changes" } else { "Diff" };
                (MainKey::Diff { root, path: file.path.clone() }, title.into())
            }
            View::Branches => (MainKey::Log { root, rev: detail.branches[ix].name.clone() }, "Log".into()),
            View::Remotes => {
                let rb = &detail.remote_branches[ix];
                (MainKey::Log { root, rev: format!("{}/{}", rb.remote, rb.name) }, "Log".into())
            }
            View::Tags => (MainKey::Show { root, rev: detail.tags[ix].name.clone() }, "Tag".into()),
            View::Commits => (MainKey::Show { root, rev: detail.commits[ix].oid.clone() }, "Patch".into()),
            View::Reflog => (MainKey::Show { root, rev: detail.reflog[ix].short_oid.clone() }, "Patch".into()),
            View::Stash => (MainKey::Stash { root, index: detail.stashes[ix].index }, "Stash".into()),
            View::Worktrees => {
                let wt = &detail.worktrees[ix];
                let text = format!(
                    "Worktree: {}\nBranch:   {}\nHEAD:     {}{}",
                    wt.path.display(),
                    wt.branch.as_deref().unwrap_or("(detached)"),
                    wt.head.as_deref().unwrap_or("-"),
                    if wt.is_current { "\n\n(this worktree)" } else { "" },
                );
                (MainKey::Message(text), "Worktree".into())
            }
            View::Submodules => {
                let sm = &detail.submodules[ix];
                let state = match sm.state {
                    '+' => "checked out at a different commit than recorded",
                    '-' => "not initialized",
                    'U' => "has merge conflicts",
                    _ => "in sync",
                };
                let text = format!("Submodule: {}\nCommit:    {}\nState:     {state}", sm.path, sm.short_oid);
                (MainKey::Message(text), "Submodule".into())
            }
            View::Repos | View::Status | View::Main => (MainKey::Overview, "".into()),
        }
    }

    pub fn sync_main(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let (key, title) = self.desired_main(store);
        let generation = store.detail_generation;
        let same_item = self.main.key.as_ref() == Some(&key);
        if same_item && (!key.is_repo_state() || self.main.generation == generation) {
            return;
        }
        self.main.key = Some(key.clone());
        self.main.generation = generation;
        self.main.title = title;
        if !same_item {
            self.main.top = 0;
            self.main.scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.main.scroll2.scroll_to_item(0, ScrollStrategy::Top);
        }
        let location = store.selected_entry().map(|e| e.location.clone());
        let file = match &key {
            MainKey::Diff { path, .. } => store
                .selected_detail()
                .and_then(|d| d.files.iter().find(|f| &f.path == path).cloned()),
            _ => None,
        };
        let git = store.git.clone();
        match key.clone() {
            MainKey::Overview => self.main.content = MainContent::Overview,
            MainKey::Status => self.main.content = MainContent::Status,
            MainKey::Message(text) => {
                self.main.content = MainContent::Text {
                    lines: Arc::new(text.lines().map(str::to_string).collect()),
                    kind: TextKind::Plain,
                }
            }
            MainKey::Diff { .. } | MainKey::Show { .. } | MainKey::Stash { .. } | MainKey::Log { .. } => {
                let Some(location) = location else { return };
                let load = cx.background_spawn(load_main(git, location, key.clone(), file));
                self.main.task = Some(cx.spawn(async move |this, cx| {
                    let content = load.await;
                    this.update(cx, |this, cx| {
                        if this.main.key.as_ref() == Some(&key) {
                            this.main.content = content;
                            cx.notify();
                        }
                    })
                    .ok();
                }));
            }
        }
    }

    fn scroll_main(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = match &self.main.content {
            MainContent::Text { lines, .. } => lines.len(),
            MainContent::Split { unstaged, staged } => unstaged.len().max(staged.len()),
            MainContent::Overview => self.store.read(cx).repos.len() + 1,
            MainContent::Status => 20,
        };
        self.main.top = self.main.top.saturating_add_signed(delta).min(len.saturating_sub(1));
        self.main.scroll.scroll_to_item(self.main.top, ScrollStrategy::Top);
        self.main.scroll2.scroll_to_item(self.main.top, ScrollStrategy::Top);
        cx.notify();
    }

    // ---- navigation actions ----------------------------------------------------------------

    pub fn next_panel(&mut self, _: &NextPanel, window: &mut Window, cx: &mut Context<Self>) {
        let current = Panel::SIDE.iter().position(|p| *p == self.main_source()).unwrap_or(0);
        self.focus_panel(Panel::SIDE[(current + 1) % Panel::SIDE.len()], window, cx);
    }

    pub fn prev_panel(&mut self, _: &PrevPanel, window: &mut Window, cx: &mut Context<Self>) {
        let current = Panel::SIDE.iter().position(|p| *p == self.main_source()).unwrap_or(0);
        let len = Panel::SIDE.len();
        self.focus_panel(Panel::SIDE[(current + len - 1) % len], window, cx);
    }

    pub fn focus_repos(&mut self, _: &FocusRepos, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Repos, window, cx);
    }
    pub fn focus_status(&mut self, _: &FocusStatus, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Status, window, cx);
    }
    pub fn focus_files(&mut self, _: &FocusFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Files, window, cx);
    }
    pub fn focus_branches(&mut self, _: &FocusBranches, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Branches, window, cx);
    }
    pub fn focus_commits(&mut self, _: &FocusCommits, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Commits, window, cx);
    }
    pub fn focus_stash(&mut self, _: &FocusStash, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Stash, window, cx);
    }
    pub fn focus_main(&mut self, _: &FocusMain, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_panel(Panel::Main, window, cx);
    }

    pub fn next_repo(&mut self, _: &NextRepo, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(View::Repos, 1, cx);
    }
    pub fn prev_repo(&mut self, _: &PrevRepo, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(View::Repos, -1, cx);
    }

    fn switch_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let panel = self.focused;
        let count = panel.tabs().len();
        if count > 1 {
            let tab = (self.tab(panel) as isize + delta).rem_euclid(count as isize) as usize;
            self.tabs.insert(panel, tab);
            self.after_change(cx);
        }
    }
    pub fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_tab(1, cx);
    }
    pub fn prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_tab(-1, cx);
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(Dialog::Help { scroll, top }) = &mut self.dialog {
            *top = top.saturating_add_signed(delta);
            scroll.scroll_to_item(*top, ScrollStrategy::Top);
            cx.notify();
            return;
        }
        match self.current_view() {
            View::Main => self.scroll_main(delta, cx),
            View::Status => {}
            view => self.move_by(view, delta, cx),
        }
    }
    pub fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }
    pub fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }
    pub fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(PAGE, cx);
    }
    pub fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-PAGE, cx);
    }
    pub fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        match self.current_view() {
            View::Main => self.scroll_main(isize::MIN / 2, cx),
            view if view.is_list() => self.move_cursor(view, |_, _| 0, cx),
            _ => {}
        }
    }
    pub fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        match self.current_view() {
            View::Main => self.scroll_main(isize::MAX / 2, cx),
            view if view.is_list() => self.move_cursor(view, |_, len| len - 1, cx),
            _ => {}
        }
    }
    pub fn scroll_main_down(&mut self, _: &ScrollMainDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_main(3, cx);
    }
    pub fn scroll_main_up(&mut self, _: &ScrollMainUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_main(-3, cx);
    }
    pub fn half_page_main_down(&mut self, _: &HalfPageMainDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_main(15, cx);
    }
    pub fn half_page_main_up(&mut self, _: &HalfPageMainUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_main(-15, cx);
    }

    pub fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused {
            Panel::Repos => self.focus_panel(Panel::Files, window, cx),
            Panel::Status | Panel::Main => {}
            _ => self.focus_panel(Panel::Main, window, cx),
        }
    }

    pub fn back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.current_view();
        if self.filters.remove(&view).is_some() {
            self.after_change(cx);
        } else if self.focused == Panel::Main {
            self.focus_panel(self.last_side, window, cx);
        } else if self.focused == Panel::Repos && !self.marked.is_empty() {
            self.marked.clear();
            cx.notify();
        }
    }

    pub fn start_filter(&mut self, _: &StartFilter, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.current_view();
        if !view.is_list() {
            return;
        }
        let current = self.filters.get(&view).cloned().unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                let value = input.read(cx).value().to_string();
                this.filters.insert(view, value);
                this.list(view).selected = 0;
                this.after_change(cx);
            }
        });
        self.filter = Some(Filter { view, input, _subscription: subscription });
        cx.notify();
    }

    pub fn toggle_help(&mut self, _: &ToggleHelp, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(
            Dialog::Help {
                scroll: UniformListScrollHandle::new(),
                top: 0,
            },
            window,
            cx,
        );
    }

    pub fn next_screen_mode(&mut self, _: &NextScreenMode, _: &mut Window, cx: &mut Context<Self>) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Normal => ScreenMode::Half,
            ScreenMode::Half => ScreenMode::Full,
            ScreenMode::Full => ScreenMode::Normal,
        };
        cx.notify();
    }
    pub fn prev_screen_mode(&mut self, _: &PrevScreenMode, _: &mut Window, cx: &mut Context<Self>) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Normal => ScreenMode::Full,
            ScreenMode::Half => ScreenMode::Normal,
            ScreenMode::Full => ScreenMode::Half,
        };
        cx.notify();
    }
    pub fn toggle_command_log(&mut self, _: &ToggleCommandLog, _: &mut Window, cx: &mut Context<Self>) {
        self.show_command_log = !self.show_command_log;
        cx.notify();
    }

    pub fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.scan(cx);
            store.load_detail(cx);
        });
    }

    pub fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // ---- dialogs --------------------------------------------------------------------------

    pub fn open_dialog(&mut self, dialog: Dialog, window: &mut Window, cx: &mut Context<Self>) {
        match &dialog {
            Dialog::Prompt { input, .. } => input.update(cx, |i, cx| i.focus(window, cx)),
            Dialog::Commit { input, .. } => input.update(cx, |i, cx| i.focus(window, cx)),
            _ => window.focus(&self.dialog_focus, cx),
        }
        self.dialog = Some(dialog);
        cx.notify();
    }

    pub fn close_dialog(&mut self, _: &CloseDialog, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.take().is_none()
            && let Some(filter) = self.filter.take()
        {
            // Escape in the filter prompt clears the filter.
            self.filters.remove(&filter.view);
            self.after_change(cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub fn confirm_dialog(&mut self, _: &ConfirmDialog, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            // Enter in the filter prompt keeps the filter and returns to the list.
            self.filter = None;
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        };
        window.focus(&self.focus, cx);
        match dialog {
            Dialog::Confirm { on_confirm, .. } => {
                if let Some(f) = on_confirm {
                    f(self, window, cx);
                }
            }
            Dialog::Prompt { input, on_submit, .. } => {
                let value = input.read(cx).value().trim().to_string();
                if let Some(f) = on_submit
                    && !value.is_empty()
                {
                    f(self, value, window, cx);
                }
            }
            Dialog::Commit { input, amend, root } => {
                let message = input.read(cx).value().trim().to_string();
                if message.is_empty() {
                    // Keep the dialog open; an empty message would abort the commit.
                    self.open_dialog(Dialog::Commit { input, amend, root }, window, cx);
                    return;
                }
                let label = if amend { "Amending" } else { "Committing" };
                self.op_on(root, label, window, cx, move |git, loc| async move {
                    ops::commit(&git, &loc, &message, amend).await
                });
            }
            Dialog::Error { .. } | Dialog::Help { .. } | Dialog::Results { .. } => {}
        }
        cx.notify();
    }

    pub fn confirm(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_confirm: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        self.open_dialog(
            Dialog::Confirm {
                title: title.into(),
                message: message.into(),
                on_confirm: Some(Box::new(on_confirm)),
            },
            window,
            cx,
        );
    }

    pub fn prompt(
        &mut self,
        title: impl Into<SharedString>,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_submit: impl FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx));
        let initial = initial.to_string();
        input.update(cx, |input, cx| input.set_value(initial, window, cx));
        self.open_dialog(
            Dialog::Prompt {
                title: title.into(),
                input,
                on_submit: Some(Box::new(on_submit)),
            },
            window,
            cx,
        );
    }

    pub fn show_error(&mut self, title: &str, err: &GitError, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(
            Dialog::Error {
                title: title.to_string().into(),
                message: err.details().into(),
            },
            window,
            cx,
        );
    }

    pub(crate) fn show_message(&mut self, title: &str, message: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(
            Dialog::Error {
                title: title.to_string().into(),
                message: message.into(),
            },
            window,
            cx,
        );
    }

    // ---- running operations ---------------------------------------------------------------

    /// Runs `op` on the repo at `root`; failures open an error popup.
    fn op_on<Fut>(
        &mut self,
        root: PathBuf,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(Git, RepoLocation) -> Fut,
    ) where
        Fut: Future<Output = Result<GitOutput, GitError>> + Send + 'static,
    {
        self.op_on_then(root, label, window, cx, op, |this, err, window, cx| {
            this.show_error(label, &err, window, cx)
        });
    }

    fn op_on_then<Fut>(
        &mut self,
        root: PathBuf,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(Git, RepoLocation) -> Fut,
        on_error: impl FnOnce(&mut Workspace, GitError, &mut Window, &mut Context<Workspace>) + 'static,
    ) where
        Fut: Future<Output = Result<GitOutput, GitError>> + Send + 'static,
    {
        let task = self.store.update(cx, |store, cx| store.run_op(&root, label, op, cx));
        cx.spawn_in(window, async move |this, cx| {
            if let Err(err) = task.await {
                this.update_in(cx, |this, window, cx| on_error(this, err, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Runs `op` on the selected repo.
    fn op<Fut>(
        &mut self,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(Git, RepoLocation) -> Fut,
    ) where
        Fut: Future<Output = Result<GitOutput, GitError>> + Send + 'static,
    {
        if let Some(root) = self.selected_root(cx) {
            self.op_on(root, label, window, cx, op);
        }
    }

    fn with_detail<R>(&self, cx: &App, f: impl FnOnce(&RepoDetail, &Workspace, &RepoStore) -> Option<R>) -> Option<R> {
        let store = self.store.read(cx);
        f(store.selected_detail()?, self, store)
    }

    fn head_is_unborn(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .selected_entry()
            .and_then(|e| e.summary.as_ref())
            .is_some_and(|s| matches!(s.head, Head::Unborn(_)))
    }

    // ---- remote & multi-repo actions ------------------------------------------------------

    pub fn fetch(&mut self, _: &Fetch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(roots) = self.marked_targets(cx) {
            return self.store.update(cx, |store, cx| store.fetch_many(roots, cx));
        }
        let Some(root) = self.selected_root(cx) else { return };
        let task = self.store.update(cx, |store, cx| store.fetch(&root, cx));
        cx.spawn_in(window, async move |this, cx| {
            if let Err(err) = task.await {
                this.update_in(cx, |this, window, cx| this.show_error("Fetch", &err, window, cx)).ok();
            }
        })
        .detach();
    }

    pub fn fetch_all(&mut self, _: &FetchAll, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.fetch_all(cx));
    }

    pub fn pull(&mut self, _: &Pull, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(roots) = self.marked_targets(cx) {
            return self.pull_repos(roots, window, cx);
        }
        self.op("Pulling", window, cx, |git, loc| async move { ops::pull(&git, &loc).await });
    }

    pub fn push(&mut self, _: &Push, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(roots) = self.marked_targets(cx) {
            return self.push_repos(roots, window, cx);
        }
        let Some(root) = self.selected_root(cx) else { return };
        let store = self.store.read(cx);
        let Some(summary) = store.entry(&root).and_then(|e| e.summary.clone()) else { return };
        if matches!(summary.head, Head::Detached(_)) {
            return self.show_message("Push", "HEAD is detached; check out a branch to push.", window, cx);
        }
        let has_upstream = matches!(summary.upstream, Upstream::Tracking { .. });
        let remote = ubergit_core::summary::primary_remote(&summary.remotes, None).map(str::to_string);
        if remote.is_none() {
            return self.show_message("Push", "This repository has no remote.", window, cx);
        }
        let push = move |force: bool| {
            let remote = remote.clone();
            move |git: Git, loc: RepoLocation| async move {
                ops::push(&git, &loc, has_upstream, remote.as_deref(), force).await
            }
        };
        let force_push = push(true);
        self.op_on_then(root.clone(), "Pushing", window, cx, push(false), move |this, err, window, cx| {
            if ops::is_push_rejected(&err) {
                this.confirm(
                    "Force push",
                    format!("{}\n\nForce push (with lease)?", err.details()),
                    window,
                    cx,
                    move |this, window, cx| this.op_on(root, "Force pushing", window, cx, force_push),
                );
            } else {
                this.show_error("Push", &err, window, cx);
            }
        });
    }

    pub fn open_in_lazygit(&mut self, _: &OpenInLazygit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected_root(cx) else { return };
        let command = self.store.read(cx).config.lazygit_command_for(&root);
        if let Err(err) = std::process::Command::new("sh").arg("-c").arg(&command).spawn() {
            self.show_message("Open in lazygit", format!("{command}\n\n{err}"), window, cx);
        }
    }

    // ---- files ------------------------------------------------------------------------------

    fn selected_file(&self, cx: &App) -> Option<ubergit_core::FileEntry> {
        self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Files, store).map(|ix| d.files[ix].clone())
        })
    }

    pub fn toggle_stage(&mut self, _: &ToggleStage, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.selected_file(cx) else { return };
        let unborn = self.head_is_unborn(cx);
        self.op("Staging", window, cx, move |git, loc| async move {
            ops::toggle_stage(&git, &loc, &file, unborn).await
        });
    }

    pub fn toggle_stage_all(&mut self, _: &ToggleStageAll, window: &mut Window, cx: &mut Context<Self>) {
        let Some(files) = self.with_detail(cx, |d, _, _| Some(d.files.clone())) else { return };
        if files.is_empty() {
            return;
        }
        let unborn = self.head_is_unborn(cx);
        self.op("Staging", window, cx, move |git, loc| async move {
            ops::toggle_stage_all(&git, &loc, &files, unborn).await
        });
    }

    pub fn commit(&mut self, _: &Commit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected_root(cx) else { return };
        let files = self.with_detail(cx, |d, _, _| Some(d.files.clone())).unwrap_or_default();
        if files.iter().any(|f| f.kind == FileKind::Unmerged) {
            return self.show_message("Commit", "Resolve merge conflicts before committing.", window, cx);
        }
        if !files.iter().any(|f| f.has_staged()) {
            return self.show_message("Commit", "No staged files. Stage with <space> or `a` first.", window, cx);
        }
        self.open_commit(root, false, String::new(), window, cx);
    }

    pub fn amend(&mut self, _: &Amend, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected_root(cx) else { return };
        if self.head_is_unborn(cx) {
            return self.show_message("Amend", "There is no commit to amend yet.", window, cx);
        }
        let Some(location) = self.store.read(cx).entry(&root).map(|e| e.location.clone()) else { return };
        let git = self.store.read(cx).git.clone();
        let message = cx.background_spawn(async move { ops::last_commit_message(&git, &location).await });
        cx.spawn_in(window, async move |this, cx| {
            let message = message.await.unwrap_or_default();
            this.update_in(cx, |this, window, cx| this.open_commit(root, true, message, window, cx)).ok();
        })
        .detach();
    }

    fn open_commit(&mut self, root: PathBuf, amend: bool, message: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .submit_on_enter(true)
                .placeholder("Summary\n\nDescription (shift-enter for a new line)")
        });
        input.update(cx, |input, cx| input.set_value(message, window, cx));
        self.open_dialog(Dialog::Commit { input, amend, root }, window, cx);
    }

    pub fn discard(&mut self, _: &Discard, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.selected_file(cx) else { return };
        let Some(root) = self.selected_root(cx) else { return };
        let what = if file.kind == FileKind::Untracked { "Delete untracked file" } else { "Discard all changes to" };
        let message = format!("{what} {}?", file.path);
        self.confirm("Discard changes", message, window, cx, move |this, window, cx| {
            this.op_on(root, "Discarding", window, cx, move |git, loc| async move {
                ops::discard(&git, &loc, &file).await
            });
        });
    }

    pub fn stash_all(&mut self, _: &StashAll, window: &mut Window, cx: &mut Context<Self>) {
        self.op("Stashing", window, cx, |git, loc| async move { ops::stash_all(&git, &loc).await });
    }

    // ---- branches, tags, commits --------------------------------------------------------

    pub fn checkout(&mut self, _: &Checkout, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.current_view();
        let target = self.with_detail(cx, |d, this, store| {
            let ix = this.selected_ix(view, store)?;
            Some(match view {
                View::Branches => CheckoutTarget::Ref(d.branches[ix].name.clone()),
                View::Remotes => CheckoutTarget::Remote(d.remote_branches[ix].clone()),
                View::Tags => CheckoutTarget::Ref(d.tags[ix].name.clone()),
                View::Commits => CheckoutTarget::Ref(d.commits[ix].oid.clone()),
                _ => return None,
            })
        });
        let Some(target) = target else { return };
        self.op("Checking out", window, cx, move |git, loc| async move {
            match target {
                CheckoutTarget::Ref(name) => ops::checkout(&git, &loc, &name).await,
                CheckoutTarget::Remote(rb) => ops::checkout_remote(&git, &loc, &rb).await,
            }
        });
    }

    pub fn checkout_previous(&mut self, _: &CheckoutPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.op("Checking out", window, cx, |git, loc| async move { ops::checkout_previous(&git, &loc).await });
    }

    pub fn new_branch(&mut self, _: &NewBranch, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.current_view();
        let start = self.with_detail(cx, |d, this, store| {
            let ix = this.selected_ix(view, store)?;
            Some(match view {
                View::Branches => (d.branches[ix].name.clone(), d.branches[ix].name.clone()),
                View::Remotes => {
                    let rb = &d.remote_branches[ix];
                    (format!("{}/{}", rb.remote, rb.name), rb.name.clone())
                }
                View::Commits => (d.commits[ix].oid.clone(), String::new()),
                _ => return None,
            })
        });
        let Some(root) = self.selected_root(cx) else { return };
        let (start, suggestion) = match start {
            Some((start, suggestion)) => (Some(start), suggestion),
            None => (None, String::new()),
        };
        let suggestion = if view == View::Remotes { suggestion } else { String::new() };
        let title = match &start {
            Some(s) => format!("New branch name (from {})", short_rev(s)),
            None => "New branch name".to_string(),
        };
        self.prompt(title, &suggestion, window, cx, move |this, name, window, cx| {
            this.op_on(root, "Creating branch", window, cx, move |git, loc| async move {
                ops::new_branch(&git, &loc, &name, start.as_deref()).await
            });
        });
    }

    pub fn delete_branch(&mut self, _: &DeleteBranch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Branches, store).map(|ix| d.branches[ix].clone())
        }) else {
            return;
        };
        if branch.is_head {
            return self.show_message("Delete branch", "You cannot delete the checked-out branch.", window, cx);
        }
        let Some(root) = self.selected_root(cx) else { return };
        let name = branch.name.clone();
        self.confirm("Delete branch", format!("Delete branch '{name}'?"), window, cx, move |this, window, cx| {
            let force_name = name.clone();
            let force_root = root.clone();
            this.op_on_then(
                root,
                "Deleting branch",
                window,
                cx,
                move |git, loc| async move { ops::delete_branch(&git, &loc, &name, false).await },
                move |this, err, window, cx| {
                    if ops::is_not_fully_merged(&err) {
                        let message = format!("'{force_name}' is not fully merged. Delete it anyway?");
                        this.confirm("Force delete branch", message, window, cx, move |this, window, cx| {
                            this.op_on(force_root, "Deleting branch", window, cx, move |git, loc| async move {
                                ops::delete_branch(&git, &loc, &force_name, true).await
                            });
                        });
                    } else {
                        this.show_error("Delete branch", &err, window, cx);
                    }
                },
            );
        });
    }

    pub fn fast_forward(&mut self, _: &FastForward, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Branches, store).map(|ix| d.branches[ix].clone())
        }) else {
            return;
        };
        self.op("Fast-forwarding", window, cx, move |git, loc| async move {
            ops::fast_forward(&git, &loc, &branch).await
        });
    }

    pub fn set_upstream(&mut self, _: &SetUpstream, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let remote = store
            .selected_entry()
            .and_then(|e| e.summary.as_ref())
            .and_then(|s| ubergit_core::summary::primary_remote(&s.remotes, None).map(str::to_string));
        let Some(branch) = self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Branches, store).map(|ix| d.branches[ix].name.clone())
        }) else {
            return;
        };
        let Some(remote) = remote else {
            return self.show_message("Set upstream", "This repository has no remote.", window, cx);
        };
        let suggestion = format!("{remote}/{branch}");
        self.prompt(format!("Upstream for '{branch}'"), &suggestion, window, cx, move |this, upstream, window, cx| {
            if let Some(root) = this.selected_root(cx) {
                this.op_on(root, "Setting upstream", window, cx, move |git, loc| async move {
                    ops::set_upstream(&git, &loc, &branch, &upstream).await
                });
            }
        });
    }

    // ---- stash ------------------------------------------------------------------------------

    fn selected_stash(&self, cx: &App) -> Option<usize> {
        self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Stash, store).map(|ix| d.stashes[ix].index)
        })
    }

    pub fn stash_apply(&mut self, _: &StashApply, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_stash(cx) else { return };
        self.op("Applying stash", window, cx, move |git, loc| async move {
            ops::stash_apply(&git, &loc, index).await
        });
    }

    pub fn stash_pop(&mut self, _: &StashPop, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_stash(cx) else { return };
        self.op("Popping stash", window, cx, move |git, loc| async move {
            ops::stash_pop(&git, &loc, index).await
        });
    }

    pub fn stash_drop(&mut self, _: &StashDrop, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_stash(cx) else { return };
        let Some(root) = self.selected_root(cx) else { return };
        self.confirm("Drop stash", format!("Drop stash@{{{index}}}?"), window, cx, move |this, window, cx| {
            this.op_on(root, "Dropping stash", window, cx, move |git, loc| async move {
                ops::stash_drop(&git, &loc, index).await
            });
        });
    }
}

enum CheckoutTarget {
    Ref(String),
    Remote(ubergit_core::RemoteBranch),
}

fn short_rev(rev: &str) -> &str {
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) { &rev[..8] } else { rev }
}

async fn load_main(git: Git, location: RepoLocation, key: MainKey, file: Option<ubergit_core::FileEntry>) -> MainContent {
    let text = |result: anyhow::Result<String>, kind: TextKind| match result {
        Ok(out) if out.trim().is_empty() => MainContent::Text {
            lines: Arc::new(vec!["(no changes)".into()]),
            kind: TextKind::Plain,
        },
        Ok(out) => MainContent::Text { lines: Arc::new(detail::to_lines(&out)), kind },
        Err(err) => MainContent::Text {
            lines: Arc::new(format!("{err:#}").lines().map(str::to_string).collect()),
            kind: TextKind::Plain,
        },
    };
    match key {
        MainKey::Diff { .. } => {
            let Some(file) = file else {
                return text(Ok(String::new()), TextKind::Plain);
            };
            match detail::file_diff(&git, &location, &file).await {
                Ok(FileDiff { unstaged: Some(u), staged: Some(s) }) => MainContent::Split {
                    unstaged: Arc::new(detail::to_lines(&u)),
                    staged: Arc::new(detail::to_lines(&s)),
                },
                Ok(FileDiff { unstaged, staged }) => {
                    text(Ok(unstaged.or(staged).unwrap_or_default()), TextKind::Diff)
                }
                Err(err) => text(Err(err), TextKind::Plain),
            }
        }
        MainKey::Show { rev, .. } => text(detail::show(&git, &location, &rev).await, TextKind::Diff),
        MainKey::Stash { index, .. } => text(detail::stash_show(&git, &location, index).await, TextKind::Diff),
        MainKey::Log { rev, .. } => text(detail::graph_log(&git, &location, &rev).await, TextKind::Log),
        MainKey::Overview | MainKey::Status | MainKey::Message(_) => MainContent::Overview,
    }
}
