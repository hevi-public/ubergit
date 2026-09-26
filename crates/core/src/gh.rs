//! Runs the GitHub CLI (`gh`) with the user's own login, so GitHub Enterprise hosts
//! work too. Like the git runner: no prompts, no pager, no colour, bounded run time.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::git::{CmdKind, CommandRecord, CommandSink};
use crate::process::{self, RunError, first_line};

/// `gh`'s exit code when a command needs a login it doesn't have (`gh help exit-codes`).
const EXIT_AUTH_REQUIRED: i32 = 4;

#[derive(Clone, Debug)]
pub struct GhCommand {
    /// Working directory; `None` keeps the app's own.
    pub cwd: Option<PathBuf>,
    pub args: Vec<OsString>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl GhCommand {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            cwd: None,
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn stdin(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn display_args(&self) -> String {
        process::display_command("gh", &self.args)
    }
}

#[derive(Clone, Debug)]
pub struct GhOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("gh is not installed")]
    NotInstalled,
    /// Exit code 4: no login for the host the command needs.
    #[error("gh is not logged in")]
    NotLoggedIn { stderr: String },
    #[error("failed to run gh: {0}")]
    Spawn(std::io::Error),
    #[error("{command} timed out after {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    /// Any other non-zero exit. `stdout` can still hold a usable answer: `gh api graphql`
    /// exits 1 when part of a query failed, but prints the data for the rest.
    #[error("{}", first_line(stderr).unwrap_or("gh exited with an error"))]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
        stdout: Vec<u8>,
    },
}

/// The `gh` CLI runner. Cheap to clone.
#[derive(Clone)]
pub struct Gh {
    program: Arc<PathBuf>,
    sink: Arc<dyn CommandSink>,
}

impl Gh {
    pub fn new(sink: Arc<dyn CommandSink>) -> Self {
        Self {
            program: Arc::new(PathBuf::from("gh")),
            sink,
        }
    }

    /// Runs `program` instead of `gh` from PATH (tests use a stand-in script).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = Arc::new(program.into());
        self
    }

    /// Runs `gh` and logs it as a network command.
    pub async fn run(&self, cmd: GhCommand) -> Result<GhOutput, GhError> {
        let started = SystemTime::now();
        let clock = Instant::now();
        let result = self.spawn(&cmd).await;
        self.sink.record(CommandRecord {
            cwd: cmd.cwd.clone().unwrap_or_default(),
            command: cmd.display_args(),
            kind: CmdKind::Network,
            started,
            duration: clock.elapsed(),
            error: result.as_ref().err().map(ToString::to_string),
        });
        result
    }

    async fn spawn(&self, cmd: &GhCommand) -> Result<GhOutput, GhError> {
        let mut std_cmd = std::process::Command::new(self.program.as_ref());
        if let Some(cwd) = &cmd.cwd {
            std_cmd.current_dir(cwd);
        }
        std_cmd
            .args(&cmd.args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("NO_COLOR", "1")
            // Terminal-style output would add colour and layout to what we parse.
            .env_remove("GH_FORCE_TTY")
            .env_remove("CLICOLOR_FORCE");

        let output = match process::run_bounded(std_cmd, cmd.stdin.as_deref(), cmd.timeout).await {
            Ok(output) => output,
            // A missing working directory fails the same way; only blame gh if it isn't that.
            Err(RunError::Spawn(err))
                if err.kind() == std::io::ErrorKind::NotFound
                    && cmd.cwd.as_ref().is_none_or(|dir| dir.is_dir()) =>
            {
                return Err(GhError::NotInstalled);
            }
            Err(RunError::Spawn(err) | RunError::Io(err)) => return Err(GhError::Spawn(err)),
            Err(RunError::TimedOut) => {
                return Err(GhError::Timeout {
                    command: cmd.display_args(),
                    timeout: cmd.timeout,
                });
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        match output.status.code() {
            Some(0) => Ok(GhOutput {
                stdout: output.stdout,
                stderr,
            }),
            Some(EXIT_AUTH_REQUIRED) => Err(GhError::NotLoggedIn { stderr }),
            code => Err(GhError::Failed {
                command: cmd.display_args(),
                code,
                stderr,
                stdout: output.stdout,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_io::block_on;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Records(Mutex<Vec<CommandRecord>>);

    impl CommandSink for Records {
        fn record(&self, record: CommandRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    /// A stand-in `gh` that runs `script`.
    fn fake_gh(dir: &Path, script: &str) -> Gh {
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh::new(Arc::new(())).with_program(path)
    }

    #[test]
    fn passes_args_and_stdin_without_prompts_and_logs_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let records = Arc::new(Records::default());
        let gh = Gh {
            sink: records.clone(),
            ..fake_gh(dir.path(), r#"echo "$GH_PROMPT_DISABLED $NO_COLOR $*"; cat"#)
        };
        let cmd = GhCommand::new(["api", "graphql", "--input", "-"]).stdin(r#"{"query":"{viewer{login}}"}"#);
        let out = block_on(gh.run(cmd)).unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "1 1 api graphql --input -\n{\"query\":\"{viewer{login}}\"}"
        );

        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "gh api graphql --input -");
        assert_eq!(records[0].kind, CmdKind::Network);
        assert_eq!(records[0].error, None);
    }

    #[test]
    fn exit_code_4_means_not_logged_in_even_before_reading_input() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4");
        let cmd = GhCommand::new(["api", "graphql", "--input", "-"]).stdin(vec![b' '; 1 << 20]);
        match block_on(gh.run(cmd)) {
            Err(GhError::NotLoggedIn { stderr }) => assert!(stderr.contains("gh auth login")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failure_keeps_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"echo '{"data":{"r0":null}}'; echo "gh: Could not resolve to a Repository" >&2; exit 1"#,
        );
        match block_on(gh.run(GhCommand::new(["api", "graphql"]))) {
            Err(err @ GhError::Failed { code: Some(1), .. }) => {
                assert_eq!(err.to_string(), "gh: Could not resolve to a Repository");
                let GhError::Failed { stdout, .. } = err else { unreachable!() };
                assert_eq!(stdout, b"{\"data\":{\"r0\":null}}\n");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_gh_is_not_installed_but_a_missing_cwd_is_not_blamed_on_gh() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Gh::new(Arc::new(())).with_program(dir.path().join("no-such-gh"));
        let result = block_on(missing.run(GhCommand::new(["--version"])));
        assert!(matches!(result, Err(GhError::NotInstalled)), "{result:?}");

        let gh = fake_gh(dir.path(), "exit 0");
        let result = block_on(gh.run(GhCommand::new(["--version"]).cwd(dir.path().join("gone"))));
        assert!(matches!(result, Err(GhError::Spawn(_))), "{result:?}");
    }

    #[test]
    fn times_out() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "sleep 10");
        let result = block_on(gh.run(GhCommand::new(["api", "graphql"]).timeout(Duration::from_millis(200))));
        assert!(matches!(result, Err(GhError::Timeout { .. })), "{result:?}");
    }
}
