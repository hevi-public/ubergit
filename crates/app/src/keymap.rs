//! Every key binding in one table: lazygit's defaults plus the Repos panel.
//!
//! Key contexts, outermost first:
//! - `Panels` wraps the whole layout: global navigation and remote ops.
//! - The focused view's own context (`Repos`, `Files`, `Branches`, ...) is deeper, so its
//!   bindings win over global ones for the same key (e.g. `f` fast-forwards in Branches).
//! - `Dialog` wraps popups; `Panels` is not on the path while one is open, so plain
//!   letter keys reach text inputs.

use gpui_kit::{KeyBinding, actions};

actions!(
    ubergit,
    [
        // navigation
        NextPanel,
        PrevPanel,
        FocusMain,
        FocusStatus,
        FocusFiles,
        FocusBranches,
        FocusCommits,
        FocusStash,
        FocusRepos,
        NextRepo,
        PrevRepo,
        NextTab,
        PrevTab,
        SelectNext,
        SelectPrev,
        PageDown,
        PageUp,
        SelectFirst,
        SelectLast,
        ScrollMainDown,
        ScrollMainUp,
        HalfPageMainDown,
        HalfPageMainUp,
        Enter,
        Back,
        StartFilter,
        ToggleHelp,
        NextScreenMode,
        PrevScreenMode,
        ToggleCommandLog,
        Refresh,
        Quit,
        // remote
        Fetch,
        FetchAll,
        Pull,
        Push,
        FastForwardAll,
        OpenInLazygit,
        // files
        ToggleStage,
        ToggleStageAll,
        Commit,
        Amend,
        Discard,
        StashAll,
        // branches
        Checkout,
        CheckoutPrevious,
        NewBranch,
        DeleteBranch,
        FastForward,
        SetUpstream,
        // stash
        StashApply,
        StashPop,
        StashDrop,
        // dialogs
        ConfirmDialog,
        CloseDialog,
    ]
);

pub struct HelpEntry {
    pub key: &'static str,
    pub context: &'static str,
    pub description: &'static str,
}

macro_rules! keymap {
    ($( $key:literal, $ctx:literal => $action:expr, $help:literal; )*) => {
        pub fn key_bindings() -> Vec<KeyBinding> {
            vec![ $( KeyBinding::new($key, $action, Some($ctx)) ),* ]
        }
        pub const HELP: &[HelpEntry] = &[
            $( HelpEntry { key: $key, context: $ctx, description: $help } ),*
        ];
    };
}

keymap! {
    // Global (lazygit defaults). An empty description marks an alias hidden in `?`.
    "h", "Panels" => PrevPanel, "Previous panel";
    "left", "Panels" => PrevPanel, "";
    "shift-tab", "Panels" => PrevPanel, "";
    "l", "Panels" => NextPanel, "Next panel";
    "right", "Panels" => NextPanel, "";
    "tab", "Panels" => NextPanel, "";
    "ctrl-r", "Panels" => FocusRepos, "Focus repos (switch repository)";
    "1", "Panels" => FocusStatus, "Focus status";
    "2", "Panels" => FocusFiles, "Focus files";
    "3", "Panels" => FocusBranches, "Focus branches";
    "4", "Panels" => FocusCommits, "Focus commits";
    "5", "Panels" => FocusStash, "Focus stash";
    "0", "Panels" => FocusMain, "Focus main view";
    "}", "Panels" => NextRepo, "Next repository (from any panel)";
    "{", "Panels" => PrevRepo, "Previous repository (from any panel)";
    "]", "Panels" => NextTab, "Next tab";
    "[", "Panels" => PrevTab, "Previous tab";
    "j", "Panels" => SelectNext, "Next item";
    "down", "Panels" => SelectNext, "";
    "k", "Panels" => SelectPrev, "Previous item";
    "up", "Panels" => SelectPrev, "";
    ".", "Panels" => PageDown, "Next page";
    ",", "Panels" => PageUp, "Previous page";
    ">", "Panels" => SelectLast, "Scroll to bottom";
    "end", "Panels" => SelectLast, "";
    "<", "Panels" => SelectFirst, "Scroll to top";
    "home", "Panels" => SelectFirst, "";
    "shift-j", "Panels" => ScrollMainDown, "Scroll main view down";
    "pagedown", "Panels" => HalfPageMainDown, "";
    "ctrl-d", "Panels" => HalfPageMainDown, "";
    "shift-k", "Panels" => ScrollMainUp, "Scroll main view up";
    "pageup", "Panels" => HalfPageMainUp, "";
    "ctrl-u", "Panels" => HalfPageMainUp, "";
    "enter", "Panels" => Enter, "View item / drill in";
    "escape", "Panels" => Back, "Back / clear filter";
    "/", "Panels" => StartFilter, "Filter list";
    "?", "Panels" => ToggleHelp, "Keybindings";
    "+", "Panels" => NextScreenMode, "Next screen mode (normal/half/full)";
    "_", "Panels" => PrevScreenMode, "Previous screen mode";
    "@", "Panels" => ToggleCommandLog, "Toggle command log";
    "shift-r", "Panels" => Refresh, "Refresh (no fetch)";
    "q", "Panels" => Quit, "Quit";
    "ctrl-c", "Panels" => Quit, "";
    "cmd-q", "Panels" => Quit, "";
    "f", "Panels" => Fetch, "Fetch";
    "p", "Panels" => Pull, "Pull";
    "shift-p", "Panels" => Push, "Push";

    // Repos
    "shift-f", "Repos" => FetchAll, "Fetch all repositories";
    "shift-u", "Repos" => FastForwardAll, "Fast-forward all clean repos that are behind";
    "o", "Repos" => OpenInLazygit, "Open in lazygit";

    // Files
    "space", "Files" => ToggleStage, "Stage / unstage";
    "a", "Files" => ToggleStageAll, "Stage / unstage all";
    "c", "Files" => Commit, "Commit";
    "shift-a", "Files" => Amend, "Amend last commit";
    "d", "Files" => Discard, "Discard changes";
    "s", "Files" => StashAll, "Stash all changes";

    // Branches, remote branches, tags
    "space", "Branches" => Checkout, "Checkout";
    "n", "Branches" => NewBranch, "New branch";
    "d", "Branches" => DeleteBranch, "Delete branch";
    "f", "Branches" => FastForward, "Fast-forward from upstream";
    "-", "Branches" => CheckoutPrevious, "Checkout previous branch";
    "u", "Branches" => SetUpstream, "Set upstream to <remote>/<name>";
    "space", "Remotes" => Checkout, "Checkout as local branch";
    "n", "Remotes" => NewBranch, "New branch from remote branch";
    "space", "Tags" => Checkout, "Checkout tag (detached)";
    "space", "Commits" => Checkout, "Checkout commit (detached)";
    "n", "Commits" => NewBranch, "New branch from commit";

    // Stash
    "space", "Stash" => StashApply, "Apply";
    "g", "Stash" => StashPop, "Pop";
    "d", "Stash" => StashDrop, "Drop";

    // Popups
    "escape", "Dialog" => CloseDialog, "Close";
    "enter", "Dialog" => ConfirmDialog, "Confirm";
    "q", "Help" => CloseDialog, "";
    "?", "Help" => CloseDialog, "";
    "j", "Help" => SelectNext, "";
    "k", "Help" => SelectPrev, "";
}

/// lazygit-style key label, e.g. `shift-j` → `J`, `ctrl-r` → `<c-r>`.
pub fn display_key(key: &str) -> String {
    if let Some(letter) = key.strip_prefix("shift-")
        && letter.len() == 1
    {
        return letter.to_uppercase();
    }
    if let Some(rest) = key.strip_prefix("ctrl-") {
        return format!("<c-{rest}>");
    }
    match key {
        "space" | "enter" | "escape" | "tab" | "up" | "down" | "left" | "right" | "home"
        | "end" | "pageup" | "pagedown" => format!("<{key}>"),
        _ => key.to_string(),
    }
}
