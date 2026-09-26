//! Runs a child process the way a background GUI needs: no controlling terminal, so
//! nothing can block on a prompt, and a time limit that kills the whole process group.
//! Shared by the `git` and `gh` runners.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Output, Stdio};
use std::time::Duration;

use futures_lite::{AsyncWriteExt as _, future};

/// Why a bounded run produced no exit status.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The program couldn't be started (e.g. not installed).
    Spawn(io::Error),
    /// Reading its output or writing its input failed.
    Io(io::Error),
    TimedOut,
}

/// Runs `cmd` in a new session, feeding it `stdin` and capturing stdout and stderr.
/// After `timeout` its whole process group is killed. A non-zero exit is not an error
/// here: the caller reads it from the returned status.
pub(crate) async fn run_bounded(
    mut cmd: std::process::Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, RunError> {
    // New session: no controlling TTY, so ssh/credential helpers can't block on a
    // prompt, and the whole process group can be killed on timeout.
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut command: async_process::Command = cmd.into();
    command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let pid = child.id() as libc::pid_t;
    let pipe = child.stdin.take();

    // Feed stdin while collecting output: a child that writes before it has read all
    // its input would otherwise deadlock against us once both pipes fill up.
    let run = future::try_zip(feed(pipe, stdin), child.output());
    let output = future::or(async { Some(run.await) }, async {
        async_io::Timer::after(timeout).await;
        None
    })
    .await;

    match output {
        Some(result) => result.map(|((), output)| output).map_err(RunError::Io),
        None => {
            // SAFETY: plain syscall; the child is its own process-group leader (setsid).
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            Err(RunError::TimedOut)
        }
    }
}

async fn feed(pipe: Option<async_process::ChildStdin>, input: Option<&[u8]>) -> io::Result<()> {
    let (Some(mut pipe), Some(input)) = (pipe, input) else {
        return Ok(());
    };
    let written = match pipe.write_all(input).await {
        Ok(()) => pipe.close().await,
        Err(err) => Err(err),
    };
    match written {
        // The child stopped reading, e.g. it failed early: its exit status says why.
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// A command line for the command log, quoting arguments that need it.
pub(crate) fn display_command(program: &str, args: &[OsString]) -> String {
    let mut out = String::from(program);
    for arg in args {
        let arg = arg.to_string_lossy();
        out.push(' ');
        if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '\'') {
            out.push('\'');
            out.push_str(&arg.replace('\'', "'\\''"));
            out.push('\'');
        } else {
            out.push_str(&arg);
        }
    }
    out
}

pub(crate) fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_io::block_on;
    use std::time::Instant;

    fn sh(script: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn large_input_and_output_do_not_deadlock() {
        let input = vec![b'x'; 1 << 20];
        let out = block_on(run_bounded(sh("cat"), Some(&input), Duration::from_secs(10))).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), input.len());
    }

    #[test]
    fn exit_status_survives_a_child_that_ignores_its_input() {
        let input = vec![b'x'; 1 << 20];
        let out = block_on(run_bounded(sh("echo nope >&2; exit 4"), Some(&input), Duration::from_secs(10))).unwrap();
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(out.stderr, b"nope\n");
    }

    #[test]
    fn times_out() {
        let started = Instant::now();
        let result = block_on(run_bounded(sh("sleep 10"), None, Duration::from_millis(200)));
        assert!(matches!(result, Err(RunError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_program_fails_to_spawn() {
        let cmd = std::process::Command::new("/nonexistent/ubergit-test-program");
        let result = block_on(run_bounded(cmd, None, Duration::from_secs(5)));
        assert!(matches!(result, Err(RunError::Spawn(err)) if err.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn quotes_display_args() {
        let args: Vec<OsString> = ["commit", "-m", "a b", "", "it's"].map(Into::into).into();
        assert_eq!(display_command("git", &args), "git commit -m 'a b' '' 'it'\\''s'");
    }
}
