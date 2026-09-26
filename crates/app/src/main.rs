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

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// What an app launched from Finder takes from the user's login shell, since it gets a
/// bare environment. PATH (/usr/bin:/bin:... otherwise), so gh and tools git shells out
/// to, like Homebrew's git-lfs or credential helpers, are found, as Zed does. The rest
/// decide which login gh uses and where gh and git read their config, so that gh isn't
/// "not logged in" here while it works in a terminal.
const LOGIN_SHELL_ENV: [&str; 8] = [
    "PATH",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
    "GH_CONFIG_DIR",
    "XDG_CONFIG_HOME",
];

/// Printed ahead of the values, so whatever the shell's startup files print can be skipped.
const LOGIN_SHELL_MARKER: &str = "__UBERGIT_ENV__";

/// How long the login shell gets to answer. The window only opens after it, and startup
/// files can be slow.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Copies [`LOGIN_SHELL_ENV`] from the login shell when launched from Finder, and returns
/// the names it set, or why it kept Finder's environment. PATH replaces Finder's; the
/// others only fill in what isn't set.
fn inherit_login_env() -> Result<Vec<&'static str>, String> {
    if std::env::var_os("TERM").is_some() {
        return Ok(Vec::new()); // started from a terminal: the environment is already the user's
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut cmd = Command::new(&shell);
    cmd.args(["-l", "-c", &login_env_script()]);
    let stdout = read_login_shell(cmd, LOGIN_SHELL_TIMEOUT).map_err(|err| format!("{shell} -l: {err}"))?;
    let home = std::env::var_os("HOME").unwrap_or_default();
    let values = parse_login_env(&stdout, &home).ok_or_else(|| format!("{shell} -l: no answer that makes sense"))?;
    let mut set = Vec::new();
    for (name, value) in values {
        if name == "PATH" || std::env::var_os(name).is_none_or(|v| v.is_empty()) {
            // SAFETY: called first thing in main, before any other thread exists.
            unsafe { std::env::set_var(name, value) };
            set.push(name);
        }
    }
    Ok(set)
}

/// Prints the marker, HOME, and each of [`LOGIN_SHELL_ENV`]. Each ends in a NUL, which no
/// value can contain, so nothing needs escaping.
fn login_env_script() -> String {
    let values: Vec<String> = ["HOME"]
        .iter()
        .chain(&LOGIN_SHELL_ENV)
        .map(|name| format!("\"${name}\""))
        .collect();
    format!("printf '%s\\0' {LOGIN_SHELL_MARKER} {}", values.join(" "))
}

/// Runs the login shell until its answer is in, then kills it with everything it started:
/// waiting for the output to end, as `output()` does, would wait for anything a startup
/// file left running in the background, like `sleep 30 &`, since it holds the pipe open.
/// Gives up after `timeout`.
fn read_login_shell(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let deadline = Instant::now() + timeout;
    // A process group of its own, so what it starts can be killed with it.
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut out = Vec::new();
    let read = loop {
        if login_env_values(&out).is_some() {
            break Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(format!("no answer after {} s", timeout.as_secs_f32()));
        }
        let mut pipe = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for a pipe this function owns.
        let ready = unsafe { libc::poll(&mut pipe, 1, left.as_millis().try_into().unwrap_or(i32::MAX)) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break Err(err.to_string());
        }
        if ready == 0 {
            continue;
        }
        // Ready, so this doesn't block.
        let mut chunk = [0; 4096];
        match stdout.read(&mut chunk) {
            // The shell is done, and left nothing holding the pipe.
            Ok(0) => break Ok(()),
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => break Err(err.to_string()),
        }
    };
    // SAFETY: plain syscall. The shell leads its group and isn't reaped yet, so the id
    // can't have been reused.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
    read.map(|()| out)
}

/// The fields after the marker in the output of [`login_env_script`], once all of them are
/// in: HOME, then [`LOGIN_SHELL_ENV`]'s.
fn login_env_values(stdout: &[u8]) -> Option<Vec<&[u8]>> {
    let mut fields = stdout.split(|&b| b == 0);
    // Startup output without a trailing NUL ends up in front of the marker.
    if !fields.by_ref().any(|field| field.ends_with(LOGIN_SHELL_MARKER.as_bytes())) {
        return None;
    }
    let values: Vec<&[u8]> = fields.collect();
    // After the last value's NUL comes one more field, so a cut-off answer is shorter.
    (values.len() > 1 + LOGIN_SHELL_ENV.len()).then_some(values)
}

/// The non-empty values of [`LOGIN_SHELL_ENV`] in the output of [`login_env_script`]. An
/// empty value counts as unset, as it does for gh and git. `None` for an answer that isn't
/// all there, or doesn't look like a POSIX shell's: nushell, say, prints `$PATH` as it is,
/// which as PATH would leave git and gh unfound. It must name `home`, the app's own HOME,
/// and a PATH with a `/` in it.
fn parse_login_env(stdout: &[u8], home: &OsStr) -> Option<Vec<(&'static str, OsString)>> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let values = login_env_values(stdout)?;
    let (shell_home, values) = values.split_first()?;
    let path = values.first()?;
    if *shell_home != home.as_bytes() || !path.contains(&b'/') || path.starts_with(b"$") {
        return None;
    }
    Some(
        LOGIN_SHELL_ENV
            .into_iter()
            .zip(values)
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (name, OsString::from_vec(value.to_vec())))
            .collect(),
    )
}

fn main() {
    let from_login_shell = inherit_login_env();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match from_login_shell {
        // Names only: some of these are tokens.
        Ok(names) if !names.is_empty() => log::debug!("from the login shell: {}", names.join(", ")),
        Ok(_) => {}
        Err(why) => log::warn!("kept Finder's environment: {why}"),
    }

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

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's `test` attribute would shadow the built-in one.
    use super::{LOGIN_SHELL_ENV, LOGIN_SHELL_MARKER, login_env_script, parse_login_env, read_login_shell};
    use std::ffi::{OsStr, OsString};
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    const HOME: &str = "/Users/me";

    /// Runs the script after output like startup files print, without the user's own
    /// startup files (zsh reads ~/.zshenv even when not a login shell, hence `-f`).
    fn run_script(shell: &str, env: &[(&str, &str)]) -> Option<Vec<(&'static str, OsString)>> {
        let mut cmd = Command::new(shell);
        if shell.ends_with("zsh") {
            cmd.arg("-f");
        }
        cmd.args(["-c", &format!("echo 'Last login: today'; printf 'no newline'; {}", login_env_script())]);
        for name in LOGIN_SHELL_ENV.iter().chain(&["BASH_ENV", "ENV"]) {
            cmd.env_remove(name);
        }
        cmd.env("HOME", HOME).envs(env.iter().copied());
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{shell}: {output:?}");
        parse_login_env(&output.stdout, OsStr::new(HOME))
    }

    #[test]
    fn reads_the_login_shells_variables() {
        let env = [
            ("PATH", "/opt/homebrew/bin:/usr/bin:/bin"),
            ("GH_TOKEN", "gho_x y\nz'\"$HOME"),
            ("GH_HOST", ""),
            ("XDG_CONFIG_HOME", "/Users/me/.config"),
        ];
        for shell in ["/bin/sh", "/bin/zsh", "/bin/bash"] {
            if !Path::new(shell).exists() {
                continue;
            }
            assert_eq!(
                run_script(shell, &env),
                Some(vec![
                    ("PATH", "/opt/homebrew/bin:/usr/bin:/bin".into()),
                    ("GH_TOKEN", "gho_x y\nz'\"$HOME".into()),
                    ("XDG_CONFIG_HOME", "/Users/me/.config".into()),
                ]),
                "{shell}"
            );
        }
    }

    /// The marker, then `fields` each ending in a NUL.
    fn answer(fields: &[&str]) -> Vec<u8> {
        let mut out = format!("{LOGIN_SHELL_MARKER}\0").into_bytes();
        for field in fields {
            out.extend(field.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn ignores_output_without_the_marker_or_cut_short() {
        let home = OsStr::new(HOME);
        assert_eq!(parse_login_env(b"", home), None);
        assert_eq!(parse_login_env(b"/usr/bin\0", home), None);
        let mut fields = vec![HOME, "/usr/bin"];
        assert_eq!(parse_login_env(&answer(&fields), home), None);
        fields.extend([""; 7]);
        assert_eq!(parse_login_env(&answer(&fields), home), Some(vec![("PATH", "/usr/bin".into())]));
    }

    #[test]
    fn ignores_an_answer_that_is_not_a_posix_shells() {
        let home = OsStr::new(HOME);
        let with = |home: &str, path: &str| {
            let mut fields = vec![home, path, "gho_token"];
            fields.extend([""; 6]);
            answer(&fields)
        };
        assert!(parse_login_env(&with(HOME, "/usr/bin:/bin"), home).is_some());
        // What a shell that doesn't expand `"$NAME"` prints.
        assert_eq!(parse_login_env(&with("$HOME", "$PATH"), home), None);
        assert_eq!(parse_login_env(&with(HOME, "$PATH"), home), None);
        // Another HOME, or no usable PATH: something went wrong, so nothing counts.
        assert_eq!(parse_login_env(&with("/Users/someone", "/usr/bin"), home), None);
        assert_eq!(parse_login_env(&with(HOME, ""), home), None);
        assert_eq!(parse_login_env(&with(HOME, "bin"), home), None);
    }

    /// zsh as a login shell with `startup` in its `.zshenv`, and HOME there too.
    fn login_zsh(dir: &Path, startup: &str) -> Command {
        std::fs::write(dir.join(".zshenv"), startup).unwrap();
        let mut cmd = Command::new("/bin/zsh");
        cmd.args(["-l", "-c", &login_env_script()]).env("ZDOTDIR", dir).env("HOME", dir);
        cmd
    }

    /// Whether `pid` is gone, waiting a little for it to be reaped.
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        // SAFETY: signal 0 only checks that the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    fn pid_in(dir: &Path) -> i32 {
        std::fs::read_to_string(dir.join("pid")).unwrap().trim().parse().unwrap()
    }

    #[test]
    fn stops_reading_once_the_answer_is_in() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Holds the output open for 30 s: `output()` would wait for it.
        let cmd = login_zsh(dir.path(), "sleep 30 & echo $! > \"$ZDOTDIR/pid\"\n");
        let started = Instant::now();
        let stdout = read_login_shell(cmd, Duration::from_secs(20)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let values = parse_login_env(&stdout, dir.path().as_os_str()).unwrap();
        assert!(values.iter().any(|(name, _)| *name == "PATH"), "{values:?}");
        // What the startup file left running went with the shell.
        assert!(gone(pid_in(dir.path())));
    }

    #[test]
    fn gives_up_on_a_shell_that_takes_too_long() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cmd = login_zsh(dir.path(), "sleep 30 & echo $! > \"$ZDOTDIR/pid\"; wait\n");
        let started = Instant::now();
        let result = read_login_shell(cmd, Duration::from_millis(500));
        assert_eq!(result, Err("no answer after 0.5 s".into()));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(gone(pid_in(dir.path())));
    }
}
