//! Git state for many repositories: discovery, status, watching, fetching. No UI.

pub mod config;
pub mod detail;
pub mod discovery;
pub mod git;
pub mod model;
pub mod ops;
pub mod patch;
pub mod summary;
pub mod watch;

pub use config::Config;
pub use git::{CmdKind, CommandRecord, CommandSink, Git, GitCommand, GitError, GitOutput};
pub use model::*;
