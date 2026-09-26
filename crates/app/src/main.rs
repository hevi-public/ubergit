//! ubergit: lazygit-style GUI for many git repositories at once.

mod batch;
mod keymap;
mod render;
mod store;
mod text;
mod theme;
mod ui_state;
mod workspace;

#[cfg(feature = "screenshot")]
mod automation;

use std::path::PathBuf;

use gpui_kit::component::{Root, Theme, ThemeMode};
use gpui_kit::*;
use ubergit_core::Config;

use crate::store::RepoStore;
use crate::ui_state::UiState;
use crate::workspace::Workspace;

const USAGE: &str = "usage: ubergit [--no-fetch] [WORKDIR]

Shows every git repository under WORKDIR (default: `workdir` from
~/.config/ubergit/config.toml, else a folder picker).

  --no-fetch   don't fetch in the background (manual f/F still work)";

/// Apps launched from Finder get a bare PATH (/usr/bin:/bin:...), so tools git shells out
/// to, like Homebrew's git-lfs or credential helpers, would be missing. Take PATH from
/// the user's login shell instead, as Zed does.
fn inherit_login_path() {
    if std::env::var_os("TERM").is_some() {
        return; // started from a terminal: PATH is already the user's
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let Ok(output) = std::process::Command::new(shell)
        .args(["-l", "-c", "printf '__PATH__%s__PATH__' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(path) = stdout.split("__PATH__").nth(1).filter(|p| !p.is_empty()) {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
    }
}

fn main() {
    inherit_login_path();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let mut workdir_arg: Option<PathBuf> = None;
    let mut no_fetch = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-fetch" => no_fetch = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("ubergit {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ if arg.starts_with('-') => {
                eprintln!("unknown option {arg}\n\n{USAGE}");
                std::process::exit(2);
            }
            _ => workdir_arg = Some(PathBuf::from(arg)),
        }
    }

    let mut config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ubergit: invalid config: {err:#}");
            std::process::exit(1);
        }
    };
    if no_fetch {
        config.auto_fetch = false;
    }
    let workdir = workdir_arg.or_else(|| config.workdir.clone());
    let workdir = match workdir.map(|w| w.canonicalize().map_err(|e| (w, e))) {
        Some(Ok(dir)) => Some(dir),
        Some(Err((dir, err))) => {
            eprintln!("ubergit: {}: {err}", dir.display());
            std::process::exit(1);
        }
        None => None,
    };

    // FSEvents and many concurrent git processes both want file descriptors.
    let _ = rlimit::increase_nofile_limit(10_240);

    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        let theme = Theme::global_mut(cx);
        theme.font_family = theme::FONT.into();
        theme.mono_font_family = theme::FONT.into();
        theme.font_size = theme::FONT_SIZE;
        cx.bind_keys(keymap::key_bindings());
        cx.on_action(|_: &keymap::Quit, cx: &mut App| cx.quit());

        match workdir {
            Some(workdir) => open_window(workdir, config, cx),
            None => {
                let paths = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Choose workdir".into()),
                });
                cx.spawn(async move |cx| {
                    let chosen = paths.await.ok().and_then(Result::ok).flatten().and_then(|mut p| p.pop());
                    cx.update(|cx| match chosen {
                        Some(workdir) => open_window(workdir, config, cx),
                        None => cx.quit(),
                    });
                })
                .detach();
            }
        }
        cx.activate(true);
    });
}

fn open_window(workdir: PathBuf, config: Config, cx: &mut App) {
    let title = format!("ubergit — {}", workdir.display());
    let state_path = ui_state::state_path();
    let saved = state_path.as_deref().map(UiState::load).unwrap_or_default();
    let (window_bounds, display_id) = ui_state::window_placement(saved.window.as_ref(), cx);
    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(900.), px(500.))),
        ..Default::default()
    };
    let window = cx
        .open_window(options, |window, cx| {
            let store = cx.new(|cx| RepoStore::new(workdir, config, cx));
            let workspace = cx.new(|cx| Workspace::new(store, saved, state_path, window, cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open window");

    #[cfg(feature = "screenshot")]
    if let Ok(script) = std::env::var("UBERGIT_SCRIPT") {
        automation::run(window.into(), script, cx);
    }
    #[cfg(not(feature = "screenshot"))]
    let _ = window;
}
