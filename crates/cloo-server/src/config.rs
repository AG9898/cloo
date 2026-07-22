//! Server-side configuration loading and reload coordination.
//!
//! `cloo-core` parses configuration *text* into a validated [`Config`]. This
//! module owns the other half: resolving the file path, reading it, and only
//! replacing a running configuration after the complete new document parsed.
//! A bad reload therefore leaves the last good configuration intact.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cloo_core::Config;
use cloo_core::config::{ConfigError, ConfigWarning, Loaded, parse};
use tokio::signal::unix::{Signal, SignalKind, signal};

/// Directory beneath a configuration root that belongs to cloo.
const CONFIG_DIR: &str = "cloo";
/// The configuration file name.
const CONFIG_FILE: &str = "config.toml";

/// A failure to find a configuration root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPathError {
    /// Neither `XDG_CONFIG_HOME` nor `HOME` named a usable root.
    NoConfigHome,
}

impl fmt::Display for ConfigPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigHome => f.write_str(
                "no configuration directory: set XDG_CONFIG_HOME or CLOO_CONFIG to a full path",
            ),
        }
    }
}

impl std::error::Error for ConfigPathError {}

/// Finds `config.toml` from explicit environment values.
///
/// `CLOO_CONFIG` wins when it is non-empty. Otherwise cloo uses
/// `$XDG_CONFIG_HOME/cloo/config.toml`, or `$HOME/.config/cloo/config.toml`
/// when the XDG variable is absent. Keeping this a pure function lets tests
/// cover the precedence without changing process-global environment variables.
///
/// # Errors
///
/// Returns [`ConfigPathError::NoConfigHome`] when no override, XDG root, or
/// home directory was supplied.
pub fn resolve_config_path(
    config_override: Option<&OsStr>,
    config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ConfigPathError> {
    if let Some(path) = config_override.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let root = match config_home.filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => match home.filter(|path| !path.is_empty()) {
            Some(path) => PathBuf::from(path).join(".config"),
            None => return Err(ConfigPathError::NoConfigHome),
        },
    };
    Ok(root.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// The one configuration file the server reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    /// Names a configuration file directly.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the file from the current process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration root can be determined.
    pub fn from_environment() -> Result<Self, ConfigPathError> {
        let path = resolve_config_path(
            env::var_os("CLOO_CONFIG").as_deref(),
            env::var_os("XDG_CONFIG_HOME").as_deref(),
            env::var_os("HOME").as_deref(),
        )?;
        Ok(Self::new(path))
    }

    /// The path read on each load or reload.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<Loaded, ConfigLoadError> {
        match fs::read_to_string(&self.path) {
            // No config is the ordinary first-run state, and is equivalent to
            // an empty document rather than an error worth warning about.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Loaded {
                config: Config::defaults(),
                warnings: Vec::new(),
            }),
            Err(source) => Err(ConfigLoadError::Read {
                path: self.path.clone(),
                source,
            }),
            Ok(text) => parse(&text).map_err(|source| ConfigLoadError::Parse {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// A configuration read that could not produce a complete validated value.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// The configuration file could not be read as UTF-8 text.
    Read {
        /// The path that failed.
        path: PathBuf,
        /// The operating-system error.
        source: io::Error,
    },
    /// The document was not valid configuration TOML.
    Parse {
        /// The path whose contents were invalid.
        path: PathBuf,
        /// The parser error.
        source: ConfigError,
    },
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    f,
                    "could not read configuration {}: {source}",
                    path.display()
                )
            }
            Self::Parse { path, source } => {
                write!(
                    f,
                    "could not load configuration {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

/// The result of attempting an atomic configuration reload.
#[derive(Debug)]
pub enum Reload {
    /// The whole document parsed, so it replaced the previous configuration.
    /// Validation warnings describe only rejected individual profile entries.
    Applied {
        /// Entries in an otherwise valid document that were ignored.
        warnings: Vec<ConfigWarning>,
    },
    /// Reading or parsing failed; the previous configuration remains active.
    Rejected {
        /// Why no new configuration was applied.
        error: ConfigLoadError,
    },
}

/// The configuration used when a process first starts.
///
/// Startup never fails solely because the optional configuration file was
/// unreadable. The caller starts with built-ins and reports the diagnostics;
/// later reloads instead retain the already active valid value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialConfig {
    /// The complete validated configuration selected for startup.
    pub config: Config,
    /// User-visible diagnostics for a rejected document or profile entries.
    pub diagnostics: Vec<String>,
}

impl Reload {
    /// Whether the configuration was replaced.
    #[must_use]
    pub const fn applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

/// A server-owned configuration that can be reloaded without a restart.
///
/// The manager has no interior mutability: its owner performs reloads in its
/// ordinary event loop. That makes the assignment after a successful parse the
/// only state transition and makes a partial apply impossible by construction.
#[derive(Debug)]
pub struct ConfigManager {
    file: ConfigFile,
    config: Config,
}

impl ConfigManager {
    /// Starts from the safe built-in configuration.
    #[must_use]
    pub fn new(file: ConfigFile) -> Self {
        Self {
            file,
            config: Config::defaults(),
        }
    }

    /// Resolves the configuration file from the current environment.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration root can be determined.
    pub fn from_environment() -> Result<Self, ConfigPathError> {
        Ok(Self::new(ConfigFile::from_environment()?))
    }

    /// The currently active, fully validated configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The file this manager will read on reload.
    #[must_use]
    pub fn file(&self) -> &ConfigFile {
        &self.file
    }

    /// Reads, validates, and atomically applies the configuration file.
    ///
    /// A missing file is a valid reset to built-ins. Any other read failure or
    /// document error returns [`Reload::Rejected`] and leaves [`Self::config`]
    /// exactly as it was.
    pub fn reload(&mut self) -> Reload {
        match self.file.load() {
            Ok(loaded) => {
                // `loaded` already holds a complete validated configuration;
                // assignment is the one atomic state transition.
                self.config = loaded.config;
                Reload::Applied {
                    warnings: loaded.warnings,
                }
            }
            Err(error) => Reload::Rejected { error },
        }
    }

    /// Waits for one `SIGHUP`, then reloads this manager's file.
    ///
    /// The manager remains the only configuration owner: the signal source
    /// only requests a reload, while this method performs the complete parse
    /// and single assignment that makes the update atomic.
    pub async fn reload_when_signalled(&mut self, watch: &mut ReloadWatch) -> Reload {
        watch.changed().await;
        self.reload()
    }
}

/// Loads the startup configuration from the current process environment.
///
/// Missing files produce the built-ins with no diagnostic. An invalid file or
/// an unavailable configuration root also falls back to built-ins, but is
/// returned in [`InitialConfig::diagnostics`] so a caller can warn instead of
/// silently pretending the requested settings applied.
#[must_use]
pub fn load_from_environment() -> InitialConfig {
    let mut manager = match ConfigManager::from_environment() {
        Ok(manager) => manager,
        Err(error) => {
            return InitialConfig {
                config: Config::defaults(),
                diagnostics: vec![error.to_string()],
            };
        }
    };

    match manager.reload() {
        Reload::Applied { warnings } => InitialConfig {
            config: manager.config().clone(),
            diagnostics: warnings
                .into_iter()
                .map(|warning| warning.to_string())
                .collect(),
        },
        Reload::Rejected { error } => InitialConfig {
            config: Config::defaults(),
            diagnostics: vec![error.to_string()],
        },
    }
}

/// An awaitable `SIGHUP` source for the server owner of a [`ConfigManager`].
///
/// The watcher intentionally does not own a manager. The daemon's event loop
/// decides when to call [`ConfigManager::reload`], so it can publish any
/// resulting changes beside other server work without a second state owner.
pub struct ReloadWatch {
    signal: Signal,
}

impl ReloadWatch {
    /// Installs the process's `SIGHUP` listener.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error when the signal stream cannot be
    /// installed.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            signal: signal(SignalKind::hangup())?,
        })
    }

    /// Waits for at least one reload request.
    ///
    /// Unix signals coalesce, which is intentional: one reload observes the
    /// complete current file and a second identical reload has no extra work.
    pub async fn changed(&mut self) {
        let _ = self.signal.recv().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_path_wins_over_every_root() {
        let path = resolve_config_path(
            Some(OsStr::new("/tmp/one.toml")),
            Some(OsStr::new("/tmp/xdg")),
            Some(OsStr::new("/home/ada")),
        )
        .expect("an override is a complete path");
        assert_eq!(path, Path::new("/tmp/one.toml"));
    }

    #[test]
    fn the_xdg_root_precedes_home() {
        let path = resolve_config_path(
            None,
            Some(OsStr::new("/var/config")),
            Some(OsStr::new("/home/ada")),
        )
        .expect("the xdg root is present");
        assert_eq!(path, Path::new("/var/config/cloo/config.toml"));
    }

    #[test]
    fn home_supplies_the_standard_config_root() {
        let path = resolve_config_path(None, None, Some(OsStr::new("/home/ada")))
            .expect("home supplies the fallback");
        assert_eq!(path, Path::new("/home/ada/.config/cloo/config.toml"));
    }

    #[test]
    fn empty_environment_values_are_absent() {
        assert_eq!(
            resolve_config_path(Some(OsStr::new("")), Some(OsStr::new("")), None),
            Err(ConfigPathError::NoConfigHome)
        );
    }
}
