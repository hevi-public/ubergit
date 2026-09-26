//! `~/.config/ubergit/config.toml`

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Directory scanned for repositories. The CLI argument overrides it.
    pub workdir: Option<PathBuf>,
    /// How deep below the workdir to look for repositories.
    pub max_depth: usize,
    /// Fetch all repos in the background.
    pub auto_fetch: bool,
    pub fetch_interval_secs: u64,
    /// Safety-net refresh of every repo's status, in case file events were missed.
    pub poll_interval_secs: u64,
    /// Shell command run by `o` to open lazygit on the selected repo; `{path}` is
    /// replaced by the repo path. Defaults to a new Terminal.app window.
    pub lazygit_command: Option<String>,
    /// Ask before `q` quits. It always asks while git is still running.
    pub confirm_quit: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workdir: None,
            max_depth: 3,
            auto_fetch: true,
            fetch_interval_secs: 300,
            poll_interval_secs: 60,
            lazygit_command: None,
            confirm_quit: true,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config/ubergit/config.toml"))
}

impl Config {
    /// Loads the config file; a missing file yields defaults.
    pub fn load() -> anyhow::Result<Self> {
        match config_path() {
            Some(path) if path.exists() => Self::load_from(&path),
            _ => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut config: Config =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        config.workdir = config.workdir.map(|p| expand_home(&p));
        Ok(config)
    }

    pub fn lazygit_command_for(&self, repo: &Path) -> String {
        let path = shell_quote(&repo.to_string_lossy());
        match &self.lazygit_command {
            Some(template) => template.replace("{path}", &path),
            None => format!(
                "osascript -e 'tell application \"Terminal\" to do script \"cd \" & quoted form of \"{}\" & \" && lazygit\"' -e 'tell application \"Terminal\" to activate'",
                repo.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"").replace('\'', "'\\''"),
            ),
        }
    }
}

pub fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let config: Config = toml::from_str("workdir = \"/x\"\nfetch_interval_secs = 60").unwrap();
        assert_eq!(config.workdir, Some(PathBuf::from("/x")));
        assert_eq!(config.fetch_interval_secs, 60);
        assert_eq!(config.max_depth, 3);
        assert!(config.confirm_quit);
        assert!(!toml::from_str::<Config>("confirm_quit = false").unwrap().confirm_quit);
        assert!(toml::from_str::<Config>("typo = 1").is_err());
    }

    #[test]
    fn lazygit_template() {
        let config = Config {
            lazygit_command: Some("wezterm start --cwd {path} lazygit".into()),
            ..Config::default()
        };
        assert_eq!(
            config.lazygit_command_for(Path::new("/a b")),
            "wezterm start --cwd '/a b' lazygit"
        );
    }
}
