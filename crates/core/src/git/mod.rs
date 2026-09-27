//! Runs the `git` CLI with settings that are safe for a background GUI:
//! write-free reads, no prompts, no pagers, no colour, bounded run time.

pub mod parse;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::process::{self, RunError, first_line};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdKind {
    /// Must not write to the repo: runs with `--no-optional-locks` and fsmonitor off.
    Read,
    /// Changes the repo (stage, commit, checkout...).
    Write,
    /// Talks to a remote (fetch, pull, push). Also changes the repo.
    Network,
}

#[derive(Clone, Debug)]
pub struct GitCommand {
    pub cwd: PathBuf,
    pub args: Vec<OsString>,
    pub kind: CmdKind,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Non-zero exit codes that still count as success (e.g. 1 for `diff --no-index`).
    pub ok_codes: Vec<i32>,
}

impl GitCommand {
    pub fn new<I, S>(cwd: impl Into<PathBuf>, kind: CmdKind, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let timeout = match kind {
            CmdKind::Read => Duration::from_secs(30),
            CmdKind::Write => Duration::from_secs(120),
            CmdKind::Network => Duration::from_secs(90),
        };
        Self {
            cwd: cwd.into(),
            args: args.into_iter().map(Into::into).collect(),
            kind,
            stdin: None,
            timeout,
            ok_codes: Vec::new(),
        }
    }

    pub fn ok_codes(mut self, codes: impl Into<Vec<i32>>) -> Self {
        self.ok_codes = codes.into();
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
        process::display_command("git", &self.args)
    }
}

#[derive(Clone, Debug)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("{command} timed out after {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    #[error("{}", first_line(stderr).unwrap_or("git exited with an error"))]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
        stdout: String,
    },
}

impl GitError {
    /// Full stderr for display in a dialog, if any.
    pub fn details(&self) -> String {
        match self {
            GitError::Failed { stderr, stdout, .. } => {
                let text = if stderr.trim().is_empty() { stdout } else { stderr };
                text.trim().to_string()
            }
            other => other.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommandRecord {
    pub cwd: PathBuf,
    pub command: String,
    pub kind: CmdKind,
    pub started: SystemTime,
    pub duration: Duration,
    /// `None` on success, otherwise a one-line error.
    pub error: Option<String>,
}

/// Receives every git invocation, for the command log.
pub trait CommandSink: Send + Sync {
    fn record(&self, record: CommandRecord);
}

impl CommandSink for () {
    fn record(&self, _: CommandRecord) {}
}

/// The git CLI runner. Cheap to clone.
#[derive(Clone)]
pub struct Git {
    sink: Arc<dyn CommandSink>,
    extra_env: Arc<Vec<(OsString, OsString)>>,
    ssh_commands: Arc<Mutex<HashMap<PathBuf, Option<String>>>>,
    read_limit: Arc<async_lock::Semaphore>,
    network_limit: Arc<async_lock::Semaphore>,
}

/// Concurrent network operations across all repos.
pub const MAX_CONCURRENT_NETWORK: usize = 4;

const SSH_OPTIONS: &str =
    "-o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=2";

impl Git {
    pub fn new(sink: Arc<dyn CommandSink>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            sink,
            extra_env: Arc::default(),
            ssh_commands: Arc::default(),
            read_limit: Arc::new(async_lock::Semaphore::new(cpus * 2)),
            network_limit: Arc::new(async_lock::Semaphore::new(MAX_CONCURRENT_NETWORK)),
        }
    }

    /// Extra environment for every git process (used by tests to isolate config).
    pub fn with_env<K: Into<OsString>, V: Into<OsString>>(
        mut self,
        env: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        self.extra_env = Arc::new(env.into_iter().map(|(k, v)| (k.into(), v.into())).collect());
        self
    }

    pub async fn run(&self, cmd: GitCommand) -> Result<GitOutput, GitError> {
        let _permit = match cmd.kind {
            CmdKind::Read => Some(self.read_limit.acquire().await),
            CmdKind::Network => Some(self.network_limit.acquire().await),
            CmdKind::Write => None,
        };
        let ssh_command = match cmd.kind {
            CmdKind::Network => self.ssh_command_for(&cmd.cwd).await,
            _ => None,
        };
        let started = SystemTime::now();
        let clock = Instant::now();
        let result = self.spawn(&cmd, ssh_command).await;
        self.sink.record(CommandRecord {
            cwd: cmd.cwd.clone(),
            command: cmd.display_args(),
            kind: cmd.kind,
            started,
            duration: clock.elapsed(),
            error: result.as_ref().err().map(ToString::to_string),
        });
        result
    }

    /// Runs a write command.
    pub async fn write<I, S>(&self, cwd: &Path, args: I) -> Result<GitOutput, GitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.run(GitCommand::new(cwd, CmdKind::Write, args)).await
    }

    /// Runs a network command.
    pub async fn network<I, S>(&self, cwd: &Path, args: I) -> Result<GitOutput, GitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.run(GitCommand::new(cwd, CmdKind::Network, args)).await
    }

    /// Runs a read command and returns stdout, or the error.
    pub async fn read<I, S>(&self, cwd: &Path, args: I) -> Result<GitOutput, GitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.run(GitCommand::new(cwd, CmdKind::Read, args)).await
    }

    async fn spawn(&self, cmd: &GitCommand, ssh_command: Option<String>) -> Result<GitOutput, GitError> {
        let mut std_cmd = std::process::Command::new("git");
        std_cmd.current_dir(&cmd.cwd);
        std_cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"]);
        match cmd.kind {
            CmdKind::Read => {
                std_cmd.args(["--no-optional-locks", "-c", "core.fsmonitor=false"]);
            }
            CmdKind::Write => {}
            CmdKind::Network => {
                std_cmd.args(["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=30"]);
            }
        }
        std_cmd.args(&cmd.args);
        std_cmd
            .env("LC_ALL", "C")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("SSH_ASKPASS_REQUIRE", "never");
        if let Some(ssh) = ssh_command {
            std_cmd.env("GIT_SSH_COMMAND", ssh);
        }
        for (key, value) in self.extra_env.iter() {
            std_cmd.env(key, value);
        }

        let output = match process::run_bounded(std_cmd, cmd.stdin.as_deref(), cmd.timeout).await {
            Ok(output) => output,
            Err(RunError::Spawn(err) | RunError::Io(err)) => return Err(GitError::Spawn(err)),
            Err(RunError::TimedOut) => {
                return Err(GitError::Timeout {
                    command: cmd.display_args(),
                    timeout: cmd.timeout,
                });
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let ok_code = output.status.code().is_some_and(|c| cmd.ok_codes.contains(&c));
        if output.status.success() || ok_code {
            Ok(GitOutput {
                stdout: output.stdout,
                stderr,
            })
        } else {
            Err(GitError::Failed {
                command: cmd.display_args(),
                code: output.status.code(),
                stderr,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        }
    }

    /// The `GIT_SSH_COMMAND` to use for network ops in `repo`: non-interactive ssh
    /// options added to the user's own ssh command, or `None` to leave theirs untouched.
    async fn ssh_command_for(&self, repo: &Path) -> Option<String> {
        if let Some(cached) = self.ssh_commands.lock().unwrap().get(repo) {
            return cached.clone();
        }
        let configured = match std::env::var("GIT_SSH_COMMAND") {
            Ok(cmd) if !cmd.trim().is_empty() => Some(cmd),
            _ => self
                .spawn(
                    &GitCommand::new(repo, CmdKind::Read, ["config", "--get", "core.sshCommand"]),
                    None,
                )
                .await
                .ok()
                .map(|out| out.stdout_str().trim().to_string())
                .filter(|cmd| !cmd.is_empty()),
        };
        let resolved = match configured {
            None if std::env::var_os("GIT_SSH").is_some() => None,
            None => Some(format!("ssh {SSH_OPTIONS}")),
            Some(cmd) if cmd == "ssh" || cmd.starts_with("ssh ") => Some(format!("{cmd} {SSH_OPTIONS}")),
            Some(_) => None,
        };
        self.ssh_commands
            .lock()
            .unwrap()
            .insert(repo.to_path_buf(), resolved.clone());
        resolved
    }
}
