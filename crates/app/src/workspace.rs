//! The root view: lazygit's panels plus the Repos column, focus, selection and actions.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::ResizableState;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{prelude::*, *};
use ubergit_core::detail::{self, FileDiff, RepoDetail};
use ubergit_core::ops::{PatchAction, StashKind};
use ubergit_core::patch::Patch;
use ubergit_core::{FileKind, Git, GitError, GitOutput, Head, RepoLocation, Upstream, ops};

use crate::batch::BatchRow;
use crate::keymap::*;
use crate::store::RepoStore;
use crate::theme::LINE_HEIGHT;

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
    /// The main view on a file's diff, with a cursor for staging lines (`enter` in Files).
    Staging,
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
            View::Staging => "Staging",
        }
    }

    pub fn is_list(self) -> bool {
        !matches!(self, View::Status | View::Main | View::Staging)
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

/// One half of a file's diff in the main view: its unstaged or its staged changes.
#[derive(Clone)]
pub struct DiffHalf {
    pub lines: Arc<Vec<String>>,
    pub patch: Arc<Patch>,
}

impl DiffHalf {
    fn new(diff: &[u8]) -> Self {
        Self {
            lines: Arc::new(detail::to_lines(&String::from_utf8_lossy(diff))),
            patch: Arc::new(Patch::parse(diff)),
        }
    }
}

#[derive(Clone)]
pub enum MainContent {
    Overview,
    Status,
    Text { lines: Arc<Vec<String>>, kind: TextKind },
    /// A changed file: its unstaged and staged changes, whichever it has.
    File { unstaged: Option<DiffHalf>, staged: Option<DiffHalf> },
}

impl MainContent {
    pub fn half(&self, staged: bool) -> Option<&DiffHalf> {
        match self {
            MainContent::File { unstaged, staged: s } => if staged { s.as_ref() } else { unstaged.as_ref() },
            _ => None,
        }
    }

    fn has_changes(&self, staged: bool) -> bool {
        self.half(staged).is_some_and(|half| half.patch.has_changes())
    }
}

pub struct MainState {
    pub key: Option<MainKey>,
    generation: u64,
    pub title: SharedString,
    pub content: MainContent,
    /// The key `content` was loaded for; it lags `key` while the new content loads.
    loaded: Option<MainKey>,
    /// Counts loads, so a load can tell whether it started after a patch was applied.
    loads: u64,
    /// The unstaged changes, or the whole view when it isn't split.
    pub scroll: UniformListScrollHandle,
    /// The staged changes.
    pub scroll2: UniformListScrollHandle,
    task: Option<Task<()>>,
}

/// The staging view's cursor, while the main view shows the diff of the file selected in
/// Files and has the focus.
#[derive(Clone, Debug, Default)]
pub struct Staging {
    /// The file being staged; `None` when the staging view isn't open.
    pub path: Option<String>,
    /// In the staged changes, rather than the unstaged ones.
    pub staged: bool,
    /// A line of the diff, always a changed one.
    pub cursor: usize,
    /// Where a range selection (`v`) started.
    pub anchor: Option<usize>,
    /// Select the block of changes around the cursor (`a`), not just its line.
    pub hunk: bool,
    /// Set while a patch is applied, then to the first load that will show its result:
    /// until that lands, the diff on screen is out of date and keys that change it wait.
    pending: Option<u64>,
}

impl Staging {
    /// The selected lines: the range, else the hunk or the line at the cursor.
    pub fn selection(&self, patch: &Patch) -> Range<usize> {
        match self.anchor {
            Some(anchor) => anchor.min(self.cursor)..anchor.max(self.cursor) + 1,
            None if self.hunk => patch.block(self.cursor).unwrap_or(self.cursor..self.cursor + 1),
            None => self.cursor..self.cursor + 1,
        }
    }
}

pub type ConfirmFn = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;
pub type SubmitFn = Box<dyn FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>;

/// One choice in a lazygit-style menu popup.
pub struct MenuItem {
    /// Picks the item directly; shown before its label.
    pub key: &'static str,
    pub label: SharedString,
    pub action: Option<ConfirmFn>,
}

impl MenuItem {
    pub fn new(
        key: &'static str,
        label: impl Into<SharedString>,
        action: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self { key, label: label.into(), action: Some(Box::new(action)) }
    }
}

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
        /// Submit an empty value too (otherwise enter on an empty prompt does nothing).
        allow_empty: bool,
    },
    Menu {
        title: SharedString,
        items: Vec<MenuItem>,
        selected: usize,
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
    },
    /// "Quit ubergit?", and which repos git is still busy in.
    Quit {
        message: SharedString,
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
    pub staging: Staging,
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

/// Page size for lists that haven't been laid out yet.
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
                loaded: None,
                loads: 0,
                scroll: UniformListScrollHandle::new(),
                scroll2: UniformListScrollHandle::new(),
                task: None,
            },
            staging: Staging::default(),
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
        if self.focused == Panel::Main && self.staging.path.is_some() {
            View::Staging
        } else {
            self.view_of(self.focused)
        }
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
            View::Status | View::Main | View::Staging => 0,
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
        // Scroll only as far as needed to keep the cursor in view, like lazygit.
        list.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
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
            if self.staging.path.take().is_some() {
                self.focused = self.last_side;
            }
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
        self.follow_staged_file(cx);
        self.after_change(cx);
    }

    fn focus_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if panel != Panel::Main {
            self.last_side = panel;
            self.staging = Staging::default();
        }
        self.focused = panel;
        window.focus(&self.focus, cx);
        self.after_change(cx);
        self.fix_staging(false, cx);
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
            View::Repos | View::Status | View::Main | View::Staging => (MainKey::Overview, "".into()),
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
            scroll_to_edge(&self.main.scroll, false);
            scroll_to_edge(&self.main.scroll2, false);
        }
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
                return self.start_load(key, cx);
            }
        }
        self.main.loaded = Some(key);
        self.fix_staging(false, cx);
    }

    /// Loads the main view's content for `key` in the background, replacing any load still
    /// running.
    fn start_load(&mut self, key: MainKey, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(location) = store.selected_entry().map(|e| e.location.clone()) else { return };
        let file = match &key {
            MainKey::Diff { path, .. } => store
                .selected_detail()
                .and_then(|d| d.files.iter().find(|f| &f.path == path).cloned()),
            _ => None,
        };
        let load = cx.background_spawn(load_main(store.git.clone(), location, key.clone(), file));
        self.main.loads += 1;
        let id = self.main.loads;
        self.main.task = Some(cx.spawn(async move |this, cx| {
            let content = load.await;
            this.update(cx, |this, cx| {
                if this.main.key.as_ref() == Some(&key) {
                    this.main.content = content;
                    this.main.loaded = Some(key);
                    let applied = this.staging.pending.is_some_and(|first| id >= first);
                    if applied {
                        this.staging.pending = None;
                    }
                    this.fix_staging(applied, cx);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Scrolls the main view (both halves of a staged/unstaged split) by `lines`.
    fn scroll_main(&mut self, lines: isize, cx: &mut Context<Self>) {
        scroll_lines(&self.main.scroll, lines);
        scroll_lines(&self.main.scroll2, lines);
        cx.notify();
    }

    fn scroll_main_to_edge(&mut self, bottom: bool, cx: &mut Context<Self>) {
        scroll_to_edge(&self.main.scroll, bottom);
        scroll_to_edge(&self.main.scroll2, bottom);
        cx.notify();
    }

    // ---- staging view ----------------------------------------------------------------------

    fn staging_scroll(&self, staged: bool) -> &UniformListScrollHandle {
        if staged { &self.main.scroll2 } else { &self.main.scroll }
    }

    /// The diff the staging cursor is in.
    fn staging_patch(&self) -> Option<Arc<Patch>> {
        self.staging.path.as_ref()?;
        Some(self.main.content.half(self.staging.staged)?.patch.clone())
    }

    /// Opens the staging view, or keeps it right after the diff reloads. Like lazygit, the
    /// cursor keeps its line if that's still a change, else it moves to the next one; when
    /// one half has nothing left, it moves to the other. The view closes when neither has
    /// anything to stage (a binary file just scrolls), back to Files if staging emptied it.
    fn fix_staging(&mut self, applied: bool, cx: &mut Context<Self>) {
        if self.focused != Panel::Main {
            return;
        }
        let Some(MainKey::Diff { path, .. }) = self.main.key.clone() else {
            self.staging = Staging::default();
            return;
        };
        if self.main.loaded != self.main.key {
            return; // Until the diff for this file arrives.
        }
        let content = &self.main.content;
        let (unstaged, staged) = (content.has_changes(false), content.has_changes(true));
        if !unstaged && !staged {
            let was_open = self.staging.path.is_some();
            self.staging = Staging::default();
            if applied && was_open {
                self.focused = self.last_side;
            }
            return;
        }
        if self.staging.path.as_deref() != Some(path.as_str()) {
            let hunk = self.store.read(cx).config.staging_hunk_mode;
            self.staging = Staging { path: Some(path), staged: !unstaged, hunk, ..Staging::default() };
        } else if !content.has_changes(self.staging.staged) {
            self.staging.staged = !self.staging.staged;
            self.staging.cursor = 0;
            self.staging.anchor = None;
        }
        let Some(patch) = self.staging_patch() else { return };
        self.staging.cursor = patch.nearest_change(self.staging.cursor).unwrap_or(0);
        if self.staging.anchor.is_some_and(|anchor| anchor >= patch.len()) {
            self.staging.anchor = None;
        }
        self.staging_scroll(self.staging.staged)
            .scroll_to_item(self.staging.cursor, ScrollStrategy::Nearest);
    }

    /// Keeps Files on the file being staged when its place in the list changes (a partly
    /// staged untracked file moves up to the tracked ones), and closes the staging view
    /// when the file has no changes left.
    fn follow_staged_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.staging.path.clone() else { return };
        let store = self.store.read(cx);
        let Some(detail) = store.selected_detail() else { return };
        let visible = self.visible(View::Files, store);
        match visible.iter().position(|&ix| detail.files[ix].path == path) {
            Some(position) => {
                let list = self.list(View::Files);
                if list.selected != position {
                    list.selected = position;
                    list.scroll.scroll_to_item(position, ScrollStrategy::Nearest);
                }
            }
            None => {
                self.staging = Staging::default();
                self.focused = self.last_side;
            }
        }
    }

    /// Moves the staging cursor to the line `to` picks, if any, and scrolls it into view.
    /// With `extend`, the move extends a range selection (starting one if needed).
    fn move_staging(&mut self, extend: bool, to: impl FnOnce(&Patch, usize) -> Option<usize>, cx: &mut Context<Self>) {
        let Some(patch) = self.staging_patch() else { return };
        if extend && self.staging.anchor.is_none() {
            self.staging.anchor = Some(self.staging.cursor);
            self.staging.hunk = false;
        }
        let Some(line) = to(&patch, self.staging.cursor) else { return };
        self.staging.cursor = line;
        let shown = match self.staging.anchor {
            Some(_) => line..line + 1,
            None => self.staging.selection(&patch),
        };
        reveal(self.staging_scroll(self.staging.staged), shown);
        cx.notify();
    }

    /// `j`/`k`: the next line, or the next hunk in hunk mode.
    fn step_staging(&mut self, forward: bool, cx: &mut Context<Self>) {
        let by_hunk = self.staging.hunk && self.staging.anchor.is_none();
        self.move_staging(
            false,
            |patch, cursor| {
                if by_hunk { patch.adjacent_block(cursor, forward) } else { patch.adjacent_change(cursor, forward) }
            },
            cx,
        );
    }

    /// `.`/`,`: the change nearest a page away.
    fn page_staging(&mut self, pages: isize, cx: &mut Context<Self>) {
        let rows = visible_rows(self.staging_scroll(self.staging.staged)) - 1;
        self.move_staging(
            false,
            |patch, cursor| {
                let target = cursor.saturating_add_signed(pages * rows.max(1));
                if pages > 0 {
                    patch.nearest_change(target)
                } else {
                    patch.adjacent_change(target + 1, false).or(patch.first_change())
                }
            },
            cx,
        );
    }

    pub fn toggle_staging_panel(&mut self, _: &ToggleStagingPanel, _: &mut Window, cx: &mut Context<Self>) {
        let other = !self.staging.staged;
        if self.staging.path.is_none() || !self.main.content.has_changes(other) {
            return;
        }
        self.staging.staged = other;
        self.staging.cursor = 0;
        self.staging.anchor = None;
        self.fix_staging(false, cx);
        cx.notify();
    }

    pub fn toggle_select_hunk(&mut self, _: &ToggleSelectHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.staging.hunk = !self.staging.hunk;
        self.staging.anchor = None;
        cx.notify();
    }

    pub fn toggle_range_select(&mut self, _: &ToggleRangeSelect, _: &mut Window, cx: &mut Context<Self>) {
        self.staging.anchor = match self.staging.anchor {
            Some(_) => None,
            None => Some(self.staging.cursor),
        };
        self.staging.hunk = false;
        cx.notify();
    }

    pub fn range_select_down(&mut self, _: &RangeSelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_staging(true, |patch, cursor| patch.adjacent_change(cursor, true), cx);
    }

    pub fn range_select_up(&mut self, _: &RangeSelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_staging(true, |patch, cursor| patch.adjacent_change(cursor, false), cx);
    }

    pub fn next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.staging.anchor = None;
        self.move_staging(false, |patch, cursor| patch.adjacent_block(cursor, true), cx);
    }

    pub fn prev_hunk(&mut self, _: &PrevHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.staging.anchor = None;
        self.move_staging(false, |patch, cursor| patch.adjacent_block(cursor, false), cx);
    }

    /// A click on a line of a file's diff: focuses the staging view with the cursor there.
    pub fn click_diff_line(&mut self, staged: bool, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused != Panel::Main {
            self.focus_panel(Panel::Main, window, cx);
        }
        let Some(patch) = self.main.content.half(staged).map(|half| half.patch.clone()) else { return };
        if self.staging.path.is_none() || !patch.has_changes() {
            return;
        }
        self.staging.staged = staged;
        self.staging.anchor = None;
        self.staging.cursor = patch.nearest_change(line).unwrap_or(0);
        cx.notify();
    }

    /// The staging view's selection as a patch for `action`, unless there's nothing to
    /// apply (or the diff on screen is about to change).
    fn build_selection(&mut self, action: PatchAction, window: &mut Window, cx: &mut Context<Self>) -> Option<Vec<u8>> {
        if self.staging.pending.is_some() {
            return None;
        }
        let patch = self.staging_patch()?;
        match patch.build(self.staging.selection(&patch), action != PatchAction::Stage) {
            Ok(built) => built,
            Err(err) => {
                self.show_message("Staging", err.to_string(), window, cx);
                None
            }
        }
    }

    /// Stages the staging view's selection from the unstaged changes, or unstages it from
    /// the staged ones.
    fn apply_selection(&mut self, action: PatchAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(built) = self.build_selection(action, window, cx) {
            self.apply_built(built, action, window, cx);
        }
    }

    fn apply_built(&mut self, built: Vec<u8>, action: PatchAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected_root(cx) else { return };
        let label = match action {
            PatchAction::Stage => "Staging",
            PatchAction::Unstage => "Unstaging",
            PatchAction::Discard => "Discarding",
        };
        self.staging.pending = Some(u64::MAX);
        self.staging.anchor = None;
        let task = self.store.update(cx, |store, cx| {
            store.run_op(&root, label, move |git, loc| async move { ops::apply_patch(&git, &loc, built, action).await }, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                // Reload the diff now rather than wait for the file list to catch up.
                Ok(_) => {
                    this.staging.pending = Some(this.main.loads + 1);
                    if let Some(key) = this.main.key.clone() {
                        this.start_load(key, cx);
                    }
                }
                Err(err) => {
                    this.staging.pending = None;
                    this.show_error(label, &err, window, cx);
                }
            })
            .ok();
        })
        .detach();
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

    /// The scrollable popup that's open, if any: `Some(None)` for a popup that doesn't scroll.
    fn popup_scroll(&self) -> Option<Option<&UniformListScrollHandle>> {
        match self.dialog.as_ref()? {
            Dialog::Help { scroll } | Dialog::Results { scroll, .. } => Some(Some(scroll)),
            _ => Some(None),
        }
    }

    /// `j`/`k` and friends: move the cursor in a list, or scroll the main view or popup.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(Dialog::Menu { items, selected, .. }) = &mut self.dialog {
            *selected = selected.saturating_add_signed(delta).min(items.len().saturating_sub(1));
            cx.notify();
            return;
        }
        if let Some(popup) = self.popup_scroll() {
            if let Some(scroll) = popup {
                scroll_lines(scroll, delta);
                cx.notify();
            }
            return;
        }
        match self.current_view() {
            View::Main => self.scroll_main(delta, cx),
            View::Staging => self.step_staging(delta > 0, cx),
            View::Status => {}
            view => self.move_by(view, delta, cx),
        }
    }

    /// `.`/`,`: a page is what fits in the view being moved, less one line of overlap.
    fn page(&mut self, pages: isize, cx: &mut Context<Self>) {
        if self.popup_scroll().is_none() && self.current_view() == View::Staging {
            return self.page_staging(pages, cx);
        }
        let rows = match self.popup_scroll() {
            Some(Some(scroll)) => visible_rows(scroll),
            Some(None) => return,
            None => match self.current_view() {
                View::Main => visible_rows(&self.main.scroll),
                view => self.lists.get(&view).map_or(PAGE, |list| visible_rows(&list.scroll)),
            },
        };
        self.step(pages * (rows - 1).max(1), cx);
    }
    pub fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }
    pub fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }
    pub fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.page(1, cx);
    }
    pub fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.page(-1, cx);
    }
    pub fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        match self.current_view() {
            View::Main => self.scroll_main_to_edge(false, cx),
            View::Staging => self.move_staging(false, |patch, _| patch.first_change(), cx),
            view if view.is_list() => self.move_cursor(view, |_, _| 0, cx),
            _ => {}
        }
    }
    pub fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        match self.current_view() {
            View::Main => self.scroll_main_to_edge(true, cx),
            View::Staging => self.move_staging(false, |patch, _| patch.adjacent_change(patch.len(), false), cx),
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
        self.scroll_main(visible_rows(&self.main.scroll) / 2, cx);
    }
    pub fn half_page_main_up(&mut self, _: &HalfPageMainUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_main(-visible_rows(&self.main.scroll) / 2, cx);
    }

    pub fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused {
            Panel::Repos => self.focus_panel(Panel::Files, window, cx),
            Panel::Status | Panel::Main => {}
            // From Files this opens the staging view, at the first change.
            _ => self.focus_panel(Panel::Main, window, cx),
        }
    }

    pub fn back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.current_view();
        if view == View::Staging && self.staging.anchor.take().is_some() {
            cx.notify();
        } else if self.filters.remove(&view).is_some() {
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

    /// `q`: asks first (unless `confirm_quit = false` and nothing is running).
    pub fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        let confirm = self.store.read(cx).config.confirm_quit;
        self.request_quit(confirm, window, cx);
    }

    /// `cmd-q`: quits at once unless git is still running, like other macOS apps.
    pub fn quit_app(&mut self, _: &QuitApp, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::Quit { .. })) {
            return cx.quit();
        }
        self.request_quit(false, window, cx);
    }

    fn request_quit(&mut self, confirm: bool, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let busy: Vec<String> = store
            .repos
            .iter()
            .filter_map(|r| r.busy.as_ref().map(|label| format!("  {}  ({label})", r.name())))
            .collect();
        if busy.is_empty() && !confirm {
            return cx.quit();
        }
        let mut message = "Quit ubergit?".to_string();
        if !busy.is_empty() {
            let repos = if busy.len() == 1 { "1 repo" } else { &format!("{} repos", busy.len()) };
            message.push_str(&format!("\n\nGit is still running in {repos}; quitting now may interrupt it:\n"));
            message.push_str(&busy.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
            if busy.len() > 8 {
                message.push_str(&format!("\n  and {} more", busy.len() - 8));
            }
        }
        self.open_dialog(Dialog::Quit { message: message.into() }, window, cx);
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
            Dialog::Prompt { input, on_submit, allow_empty, .. } => {
                let value = input.read(cx).value().trim().to_string();
                if let Some(f) = on_submit
                    && (allow_empty || !value.is_empty())
                {
                    f(self, value, window, cx);
                }
            }
            Dialog::Menu { mut items, selected, .. } => {
                if let Some(action) = items.get_mut(selected).and_then(|item| item.action.take()) {
                    action(self, window, cx);
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
            Dialog::Quit { .. } => cx.quit(),
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
                allow_empty: false,
            },
            window,
            cx,
        );
    }

    /// A prompt whose answer may be empty; `placeholder` says what empty means.
    pub fn optional_prompt(
        &mut self,
        title: impl Into<SharedString>,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_submit: impl FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        self.open_dialog(
            Dialog::Prompt {
                title: title.into(),
                input,
                on_submit: Some(Box::new(on_submit)),
                allow_empty: true,
            },
            window,
            cx,
        );
    }

    pub fn menu(&mut self, title: impl Into<SharedString>, items: Vec<MenuItem>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(Dialog::Menu { title: title.into(), items, selected: 0 }, window, cx);
    }

    /// Runs a menu item picked by its key or a click.
    pub fn pick_menu_item(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Dialog::Menu { selected, .. }) = &mut self.dialog {
            *selected = ix;
            self.confirm_dialog(&ConfirmDialog, window, cx);
        }
    }

    /// A menu item's own key picks it, like lazygit's menus.
    pub fn menu_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Menu { items, .. }) = &self.dialog else { return };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        if let Some(ix) = items.iter().position(|item| item.key == keystroke.key) {
            cx.stop_propagation();
            self.pick_menu_item(ix, window, cx);
        }
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
        if self.current_view() == View::Staging {
            let action = if self.staging.staged { PatchAction::Unstage } else { PatchAction::Stage };
            return self.apply_selection(action, window, cx);
        }
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
        if self.current_view() == View::Staging {
            // lazygit: `d` on staged changes unstages them; on unstaged ones it discards.
            if self.staging.staged {
                return self.apply_selection(PatchAction::Unstage, window, cx);
            }
            // Built now, so a reload while the popup is open can't change what goes.
            let Some(built) = self.build_selection(PatchAction::Discard, window, cx) else { return };
            let what = if self.staging.hunk && self.staging.anchor.is_none() { "hunk" } else { "lines" };
            let path = self.staging.path.clone().unwrap_or_default();
            let message = format!("Discard the selected {what} in {path}? This can't be undone.");
            return self.confirm("Discard changes", message, window, cx, move |this, window, cx| {
                this.apply_built(built, PatchAction::Discard, window, cx)
            });
        }
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
        self.stash(StashKind::All, window, cx);
    }

    pub fn stash_options(&mut self, _: &StashOptions, window: &mut Window, cx: &mut Context<Self>) {
        let item = |key, label: &str, kind| MenuItem::new(key, label.to_string(), move |this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| this.stash(kind, window, cx));
        let items = vec![
            item("a", "Stash all changes, including untracked files", StashKind::All),
            item("i", "Stash all changes but keep the staged ones in place (keep index)", StashKind::KeepIndex),
            item("t", "Stash changes to tracked files only", StashKind::Tracked),
            item("s", "Stash staged changes only", StashKind::Staged),
        ];
        self.menu("Stash options", items, window, cx);
    }

    /// Asks for a message, then stashes.
    fn stash(&mut self, kind: StashKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected_root(cx) else { return };
        let files = self.with_detail(cx, |d, _, _| Some(d.files.clone())).unwrap_or_default();
        let nothing = match kind {
            StashKind::Staged => !files.iter().any(|f| f.has_staged()),
            StashKind::Tracked => files.iter().all(|f| f.kind == FileKind::Untracked),
            StashKind::All | StashKind::KeepIndex => files.is_empty(),
        };
        if nothing {
            let what = match kind {
                StashKind::Staged => "No staged changes to stash.",
                StashKind::Tracked => "No changes to tracked files to stash.",
                StashKind::All | StashKind::KeepIndex => "Nothing to stash.",
            };
            return self.show_message("Stash", what, window, cx);
        }
        let title = match kind {
            StashKind::All => "Stash all changes",
            StashKind::KeepIndex => "Stash all changes, keep staged",
            StashKind::Tracked => "Stash tracked changes",
            StashKind::Staged => "Stash staged changes",
        };
        self.optional_prompt(title, "Message (empty for git's default)", window, cx, move |this, message, window, cx| {
            this.op_on(root, "Stashing", window, cx, move |git, loc| async move {
                ops::stash(&git, &loc, kind, Some(&message)).await
            });
        });
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

    pub fn rename_stash(&mut self, _: &RenameStash, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stash) = self.with_detail(cx, |d, this, store| {
            this.selected_ix(View::Stash, store).map(|ix| d.stashes[ix].clone())
        }) else {
            return;
        };
        let Some(root) = self.selected_root(cx) else { return };
        // Start from the current message when the user wrote one (not git's `WIP on ...`).
        let initial = match ops::stash_subject_parts(&stash.subject) {
            Some((_, message)) if stash.subject.starts_with("On ") => message.to_string(),
            _ => String::new(),
        };
        let title = format!("Rename stash@{{{}}}", stash.index);
        self.prompt(title, &initial, window, cx, move |this, message, window, cx| {
            let task = this.store.update(cx, |store, cx| {
                store.run_op(&root, "Renaming stash", move |git, loc| async move {
                    ops::stash_rename(&git, &loc, &stash, &message).await
                }, cx)
            });
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                this.update_in(cx, |this, window, cx| match result {
                    // The renamed stash is now stash@{0}: keep the cursor on it.
                    Ok(_) => {
                        this.list(View::Stash).selected = 0;
                        this.after_change(cx);
                    }
                    Err(err) => this.show_error("Rename stash", &err, window, cx),
                })
                .ok();
            })
            .detach();
        });
    }

    pub fn branch_from_stash(&mut self, _: &BranchFromStash, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_stash(cx) else { return };
        let Some(root) = self.selected_root(cx) else { return };
        self.prompt(format!("New branch from stash@{{{index}}}"), "", window, cx, move |this, name, window, cx| {
            if !ops::is_valid_branch_name(&name) {
                return this.show_message("New branch", format!("'{name}' is not a valid branch name."), window, cx);
            }
            this.op_on(root, "Creating branch", window, cx, move |git, loc| async move {
                ops::stash_branch(&git, &loc, index, &name).await
            });
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

/// Scrolls a list by `lines` rows (negative is up) from wherever it is now, so the keys
/// and the mouse wheel move one shared position. (`scroll_to_item` can't do this: it does
/// nothing while the target row is already on screen.)
fn scroll_lines(handle: &UniformListScrollHandle, lines: isize) {
    let mut state = handle.0.borrow_mut();
    // A pending scroll-to-item would override this at the next layout.
    state.deferred_scroll_to_item = None;
    let base = &state.base_handle;
    let offset = base.offset();
    let y = (offset.y - LINE_HEIGHT * lines as f32).clamp(-base.max_offset().y, px(0.));
    base.set_offset(point(offset.x, y));
}

fn scroll_to_edge(handle: &UniformListScrollHandle, bottom: bool) {
    let mut state = handle.0.borrow_mut();
    state.deferred_scroll_to_item = None;
    let base = &state.base_handle;
    let y = if bottom { -base.max_offset().y } else { px(0.) };
    base.set_offset(point(base.offset().x, y));
}

/// Scrolls a list as little as possible to show `rows` (their top, if they don't all
/// fit), going by its last layout.
fn reveal(handle: &UniformListScrollHandle, rows: Range<usize>) {
    let mut state = handle.0.borrow_mut();
    let height = state.base_handle.bounds().size.height;
    if height <= px(0.) {
        drop(state);
        return handle.scroll_to_item(rows.start, ScrollStrategy::Nearest);
    }
    state.deferred_scroll_to_item = None;
    let base = &state.base_handle;
    let offset = base.offset();
    let top = -offset.y;
    let (start, end) = (LINE_HEIGHT * rows.start as f32, LINE_HEIGHT * rows.end as f32);
    let top = if start < top {
        start
    } else if end > top + height {
        if end - height < start { end - height } else { start }
    } else {
        return;
    };
    base.set_offset(point(offset.x, (-top).clamp(-base.max_offset().y, px(0.))));
}

/// Rows that fit in a list's viewport, as of its last layout.
fn visible_rows(handle: &UniformListScrollHandle) -> isize {
    let height = handle.0.borrow().base_handle.bounds().size.height;
    ((height / LINE_HEIGHT) as isize).max(1)
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
            // The file list may be a moment behind (e.g. just after staging a line).
            let file = match detail::file_status(&git, &location, &file).await {
                Ok(Some(fresh)) => fresh,
                Ok(None) => return text(Ok(String::new()), TextKind::Plain),
                Err(_) => file,
            };
            if file.kind == FileKind::Unmerged {
                // A combined diff: nothing in it can be staged line by line.
                let diff = detail::file_diff(&git, &location, &file).await;
                return text(diff.map(|d| String::from_utf8_lossy(&d.unstaged.unwrap_or_default()).into_owned()), TextKind::Diff);
            }
            match detail::file_diff(&git, &location, &file).await {
                Ok(FileDiff { unstaged: None, staged: None }) => text(Ok(String::new()), TextKind::Plain),
                Ok(FileDiff { unstaged, staged }) => MainContent::File {
                    unstaged: unstaged.as_deref().map(DiffHalf::new),
                    staged: staged.as_deref().map(DiffHalf::new),
                },
                Err(err) => text(Err(err), TextKind::Plain),
            }
        }
        MainKey::Show { rev, .. } => text(detail::show(&git, &location, &rev).await, TextKind::Diff),
        MainKey::Stash { index, .. } => text(detail::stash_show(&git, &location, index).await, TextKind::Diff),
        MainKey::Log { rev, .. } => text(detail::graph_log(&git, &location, &rev).await, TextKind::Log),
        MainKey::Overview | MainKey::Status | MainKey::Message(_) => MainContent::Overview,
    }
}
