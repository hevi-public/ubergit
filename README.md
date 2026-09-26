# ubergit

A lazygit-style desktop GUI (Rust + [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui))
for working across many git repositories at once, e.g. a microservice workdir.

Point it at a directory; it finds every repo underneath and shows their live state:
branch or detached HEAD, dirty files, ahead/behind upstream, ahead/behind the
remote's default branch (`origin/HEAD` → `origin/main` / `origin/master`), stashes,
in-progress rebase/merge, and last fetch. The layout and keys are lazygit's, with one
extra panel on the left listing the repos.

```
[⌘R] Repos │ [1] Status              │ [0] Main: overview / diff / log / patch
           │ [2] Files - Worktrees…  │
           │ [3] Branches - Remotes… │
           │ [4] Commits - Reflog    ├──────────────────────────────────────
           │ [5] Stash               │ Command log
```

Drag the gaps between panels to resize them: the three columns, the side panels below
Status, and the main view against the command log.

The layout survives restarts. Panel sizes, the window's size, position and display, the
screen mode, whether the command log shows, the focused panel, each panel's tab, and each
workdir's selected repo are saved on quit and every 30 s to
`~/Library/Application Support/ubergit/state.json`. Delete the file to reset them. If the saved
display is gone or the window would be off-screen, the window opens centred instead.

## Run

```sh
cargo run --release -p ubergit -- ~/work/services
scripts/bundle-mac.sh          # → target/release/ubergit.app (ad-hoc signed)
```

Options: `--no-fetch` disables background fetching.

## How it stays live

- **File watching**: one recursive FSEvents watch on the workdir. Events are routed to
  their repo and coalesced (250 ms quiet, 2 s max). Changes under `node_modules`, `target`
  and other gitignored paths are dropped.
- **Auto-fetch**: every 5 minutes, at most 4 repos at a time. It never prompts: the terminal
  prompt is off and ssh runs with `BatchMode`. Failures show as `fetch failed` on the repo.
- **Safety-net poll**: every repo's status is recomputed every 60 s.

Everything goes through the `git` CLI, so your hooks, signing, credential helpers,
`includeIf` and LFS behave exactly as in a terminal. Background reads use
`--no-optional-locks` and never write to your repos.

## Keys

lazygit defaults: `h`/`l` or `tab` switch panels, `1`–`5` jump to a panel, `0` jumps to the main view,
`j`/`k` move, `[`/`]` switch tabs, `/` filters, `?` lists all keys, `+`/`_` change the screen mode, `q` quits
(press `q` again, or `enter`, to confirm; `⌘Q` quits at once unless git is still running).

| Where | Keys |
|---|---|
| Anywhere | `⌘R` repos panel (`ctrl-r` works too) · `{`/`}` previous/next repo without leaving the panel · `f` fetch · `p` pull · `P` push (asks before force-with-lease) · `R` rescan · `@` toggle command log |
| Repos | `enter` open the repo's files · `space` mark · `a` mark all · `c` check out a branch by name · `n` new branch · `m` default branch + fast-forward · `F` fetch all · `U` fast-forward repos that are behind · `o` open in lazygit |
| Files | `space` stage/unstage · `enter` stage lines · `a` stage all · `c` commit · `A` amend · `d` discard · `s` stash (asks for a message) · `S` stash options |
| Staging (`enter` on a file) | `space` stage/unstage the selection · `d` discard it (in staged changes: unstage it) · `a` hunk or line selection · `v` range · `shift-↑`/`shift-↓` extend the range · `h`/`l` or `←`/`→` previous/next hunk · `tab` other half · `c` commit · `esc` back |
| Branches | `space` checkout · `n` new · `d` delete · `f` fast-forward · `-` previous branch · `u` set upstream |
| Remotes / Tags / Commits | `space` checkout · `n` new branch from it |
| Stash | `space` apply · `g` pop · `d` drop · `r` rename · `n` new branch from stash |

## Staging lines

`enter` on a file opens lazygit's staging view in the main panel: unstaged changes above staged
ones, with a cursor. Like lazygit 0.62, it starts by selecting hunks, the run of changed lines
around the cursor. `a` switches to single lines, and `v` selects a range. `j`/`k` move to the next
hunk or line. `space` stages the selection, or unstages it in the staged half, and `d` discards it
after asking. Clicking a line puts the cursor there.

Untracked, new, deleted and renamed files work too: staging part of a new file adds just those
lines, and unstaging lines of a rename keeps the rename. Binary files, submodules and files with
conflicts have no lines to pick; stage them whole from Files.

## Working across repos

Mark repos in the Repos panel with `space` (or cmd-click), or mark every listed repo with `a`.
The count shows in the panel title, and `esc` clears the marks. Actions started from the Repos
panel then run on every marked repo at once. With nothing marked, they run on the selected repo.

- `c` checks out a branch by name. It uses the local branch, or creates one tracking
  `origin/<name>`.
- `n` creates the same new branch from HEAD in each repo, for a feature that spans services.
- `m` checks out the default branch and fast-forwards it from the last fetch. This is the
  quickest way back to a clean `main` everywhere.
- `f` / `p` / `P` fetch, pull or push each marked repo. Push asks first, and it never
  force-pushes more than one repo: to force-push, press `P` on that repo alone.
- `U` fast-forwards the marked repos, or, with nothing marked, every repo that can be.

Nothing is half-done. Repos that are busy, bare, mid-rebase or mid-merge are skipped with a
reason, and so are repos with uncommitted changes for `c`, `m` and `U`. The others run in
parallel, and a popup fills in each repo's result as it finishes.

## Config

`~/.config/ubergit/config.toml` (all optional):

```toml
workdir = "~/work/services"   # used when no argument is given (else a folder picker)
max_depth = 3                 # how deep to look for repos
auto_fetch = true
fetch_interval_secs = 300
poll_interval_secs = 60
lazygit_command = "wezterm start --cwd {path} lazygit"   # default: new Terminal.app window
confirm_quit = true           # q asks first; ⌘Q quits at once. Both ask while git is running
staging_hunk_mode = true      # the staging view selects hunks first (false: lines); a switches
```

## Development

```sh
cargo test --workspace                      # parsers + integration tests on real repos
scripts/make-fixtures.sh /tmp/fixtures      # ~20 repos in every interesting state
cargo run -p ubergit -- /tmp/fixtures
```

`crates/core` holds discovery, the git runner and parsers, summaries, operations and the
watcher. It has no UI dependency. `crates/app` holds the GPUI UI and depends on
`gpui-kit =0.6.6`, which pins `gpui-pre =0.3.6`. Upgrade both together.

To check the UI without a person at the keyboard, build with `--features screenshot`. Then set
`UBERGIT_SCRIPT`, e.g. `wait 2000; keys j 2; shot /tmp/a.png; type msg; key enter; quit`. It
replays the keystrokes and saves offscreen PNGs of the real Metal-rendered frame. Scripted runs
don't read or write `state.json` unless `UBERGIT_STATE` names a state file to use.

Not in v1: interactive rebase, cherry-pick, a merge-conflict UI, custom patches, a commit
graph, and custom keybindings.
