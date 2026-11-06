//! Everything the CLI needs from the outside world, gathered in one place so
//! tests can swap in fakes (no network, fixed clock, fixed env and files).

use crate::config::SharedConfig;
use crate::http::Transport;
use crate::loader::Loader;
use crate::oauth::CallbackFactory;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::time::Duration;

pub type EnvLookup<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;
pub type Prompt<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

pub struct Context<'a> {
    pub loader: Loader,
    pub config: SharedConfig,
    pub env: EnvLookup<'a>,
    pub transport: &'a dyn Transport,
    pub now: Box<dyn Fn() -> DateTime<Utc> + 'a>,
    pub uuid: Box<dyn Fn() -> String + 'a>,
    /// Ask the user for a line of input (MFA codes). `None` if unavailable.
    pub prompt: Prompt<'a>,
    pub sleep: Box<dyn Fn(Duration) + 'a>,
    /// Open a URL in a browser; returns false if that was not possible.
    pub open_browser: Box<dyn Fn(&str) -> bool + 'a>,
    /// Whether stdout is a terminal (enables the pager and table colors).
    pub stdout_is_tty: bool,
    /// Terminal width for `--output table`; `None` when stdout is not a
    /// terminal (awscli then assumes 80 columns).
    pub terminal_width: Option<usize>,
    /// Cryptographically secure random bytes (PKCE, DPoP keys).
    pub random: Box<dyn Fn(usize) -> Vec<u8> + 'a>,
    /// Starts the local OAuth redirect receiver (`login`, `sso login`).
    pub callback: CallbackFactory<'a>,
}

impl Context<'_> {
    pub fn env_var(&self, key: &str) -> Option<String> {
        (self.env)(key).filter(|v| !v.is_empty())
    }

    /// `~` as seen through the injected environment.
    pub fn home(&self) -> PathBuf {
        self.env_var("HOME").map(PathBuf::from).unwrap_or_default()
    }

    /// The shared config file (`AWS_CONFIG_FILE`, default `~/.aws/config`).
    pub fn config_file(&self) -> PathBuf {
        match self.env_var("AWS_CONFIG_FILE") {
            Some(p) => match p.strip_prefix("~/") {
                Some(rest) => self.home().join(rest),
                None => PathBuf::from(p),
            },
            None => self.aws_dir("config"),
        }
    }

    /// `~/.aws/<rest>`.
    pub fn aws_dir(&self, rest: &str) -> PathBuf {
        self.home().join(".aws").join(rest)
    }
}
