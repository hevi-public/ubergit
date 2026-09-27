//! Runs a child process the way a background GUI needs: no controlling terminal, so
//! nothing can block on a prompt, and a time limit that kills the whole process group.
//! Shared by the `git` and `gh` runners.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::pin::pin;
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
    // its input would otherwise deadlock against us once both pipes fill up. The timer
    // covers the write too, so a child that never reads can't hang us.
    let mut run = pin!(future::try_zip(feed(pipe, stdin), child.output()));
    // Declared after `run` so it drops first, while `run` still owns the unreaped child
    // and the group id can't have been reused.
    let mut group = GroupKill(Some(pid));
    let output = future::or(async { Some(run.as_mut().await) }, async {
        async_io::Timer::after(timeout).await;
        None
    })
    .await;

    match output {
        Some(Ok(((), output))) => {
            group.disarm();
            Ok(output)
        }
        Some(Err(err)) => Err(RunError::Io(err)),
        None => Err(RunError::TimedOut),
    }
}

/// Kills a child's whole process group when its run is abandoned: on timeout, on an I/O
/// error, or when the caller drops the future. Dropping the `Child` alone would only kill
/// the leader, leaving grandchildren like ssh or credential helpers running.
struct GroupKill(Option<libc::pid_t>);

impl GroupKill {
    /// The child has exited and been reaped, so its pid may be reused: leave it be.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: plain syscall; the child is its own process-group leader (setsid).
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
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
    use std::path::Path;
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

    fn read_pid(path: &Path) -> libc::pid_t {
        for _ in 0..100 {
            if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()) {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no pid in {}", path.display());
    }

    /// Waits for `pid` to disappear; kills it (so it can't outlive the test) if it doesn't.
    fn is_gone(pid: libc::pid_t) -> bool {
        for _ in 0..100 {
            // SAFETY: plain syscalls on a pid the test started.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        false
    }

    /// A script whose `sleep` grandchild writes its pid to `pidfile`, then `tail`.
    fn with_grandchild(pidfile: &Path, tail: &str) -> std::process::Command {
        sh(&format!("sleep 30 & echo $! > '{}'; {tail}", pidfile.display()))
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let started = Instant::now();
        let result = block_on(run_bounded(with_grandchild(&pidfile, "wait"), None, Duration::from_millis(300)));
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(is_gone(read_pid(&pidfile)), "grandchild survived the timeout");
    }

    #[test]
    fn times_out_while_the_child_ignores_its_input() {
        let input = vec![b'x'; 1 << 20];
        let started = Instant::now();
        let result = block_on(run_bounded(sh("sleep 30"), Some(&input), Duration::from_millis(300)));
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dropping_the_run_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let run = run_bounded(with_grandchild(&pidfile, "wait"), None, Duration::from_secs(30));
        // The caller gives up first, e.g. a newer load replaces this one.
        let finished = block_on(future::or(async { Some(run.await) }, async {
            async_io::Timer::after(Duration::from_millis(300)).await;
            None
        }));
        assert!(finished.is_none());
        assert!(is_gone(read_pid(&pidfile)), "grandchild survived the dropped run");
    }

    #[test]
    fn runs_in_its_own_session_without_a_terminal() {
        let out = block_on(run_bounded(sh("echo $$; ps -o pgid= -o tty= -p $$"), None, Duration::from_secs(10))).unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3, "{text}");
        assert_eq!(fields[0], fields[1], "not a process-group leader: {text}");
        assert_eq!(fields[2], "??", "has a terminal: {text}");
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
