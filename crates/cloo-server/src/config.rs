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
    /// Validation warnings describe only individual entries — a profile or a
    /// key binding — that were rejected on their own.
    Applied {
        /// Entries in an otherwise valid document that were ignored.
        warnings: Vec<ConfigWarning>,
    },
    /// Reading or parsing failed; the previous configuration remains active.
    Rejected {
        /// Why no new configuration was applied.
        error: ConfigLoadError,
    },
    /// This configuration came from no file, so a reload had nothing to
    /// re-read and the active value stands.
    ///
    /// Distinct from [`Self::Applied`] on purpose: a manager with no file is
    /// not a manager whose file vanished. Re-reading nothing must never reset a
    /// caller-supplied configuration to the built-ins, and it must never be
    /// published as a new revision, because nothing changed.
    Detached,
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
    /// The file this configuration was resolved from, or `None` when no
    /// configuration root could be determined. A rejected document still names
    /// its file: fixing it and sending `SIGHUP` must be enough.
    pub file: Option<ConfigFile>,
    /// User-visible diagnostics for a rejected document or for the individual
    /// profile or key entries that were dropped.
    pub diagnostics: Vec<String>,
}

impl InitialConfig {
    /// Turns the startup result into the manager a daemon reloads from.
    ///
    /// The active configuration is carried across rather than re-read, and the
    /// file — if there was one — is what a later `SIGHUP` re-reads.
    #[must_use]
    pub fn into_manager(self) -> ConfigManager {
        match self.file {
            Some(file) => ConfigManager::preloaded(file, self.config),
            None => ConfigManager::detached(self.config),
        }
    }
}

impl Reload {
    /// Whether the configuration was replaced.
    #[must_use]
    pub const fn applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// The user-visible diagnostics this outcome should report, in order.
    ///
    /// A rejected reload renders one line saying which file was refused and
    /// why; an applied one renders a line per entry it had to drop. Producing
    /// them here rather than at each call site is what lets a daemon and the
    /// startup path report a reload in the same words — and lets the process
    /// that owns a terminal decide where those words go, since a library never
    /// writes to one itself.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<String> {
        match self {
            Self::Applied { warnings } => warnings.iter().map(ToString::to_string).collect(),
            Self::Rejected { error } => vec![error.to_string()],
            Self::Detached => Vec::new(),
        }
    }
}

/// A server-owned configuration that can be reloaded without a restart.
///
/// The manager has no interior mutability: its owner performs reloads in its
/// ordinary event loop. That makes the assignment after a successful parse the
/// only state transition and makes a partial apply impossible by construction.
#[derive(Debug)]
pub struct ConfigManager {
    /// `None` for a configuration that came from no file — see
    /// [`ConfigManager::detached`].
    file: Option<ConfigFile>,
    config: Config,
}

impl ConfigManager {
    /// Starts from the safe built-in configuration.
    #[must_use]
    pub fn new(file: ConfigFile) -> Self {
        Self {
            file: Some(file),
            config: Config::defaults(),
        }
    }

    /// Adopts an already validated configuration read from `file`.
    ///
    /// Startup loads the document before the socket is bound, so the daemon
    /// that inherits it must not read the same file a second time — two reads
    /// could disagree, and the second one would be the unreported answer.
    #[must_use]
    pub fn preloaded(file: ConfigFile, config: Config) -> Self {
        Self {
            file: Some(file),
            config,
        }
    }

    /// Adopts a configuration that came from no file at all.
    ///
    /// Used where a caller supplies a configuration directly — a test fixture,
    /// or a host with no resolvable configuration root. A reload then has
    /// nothing to re-read and answers [`Reload::Detached`], leaving the
    /// supplied value in force rather than resetting it to the built-ins.
    #[must_use]
    pub const fn detached(config: Config) -> Self {
        Self { file: None, config }
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

    /// The file this manager will read on reload, if it has one.
    #[must_use]
    pub fn file(&self) -> Option<&ConfigFile> {
        self.file.as_ref()
    }

    /// Reads, validates, and atomically applies the configuration file.
    ///
    /// A missing file is a valid reset to built-ins. Any other read failure or
    /// document error returns [`Reload::Rejected`] and leaves [`Self::config`]
    /// exactly as it was. A manager with no file answers [`Reload::Detached`]
    /// and changes nothing.
    pub fn reload(&mut self) -> Reload {
        let Some(file) = self.file.as_ref() else {
            return Reload::Detached;
        };
        match file.load() {
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
                file: None,
                diagnostics: vec![error.to_string()],
            };
        }
    };

    let reload = manager.reload();
    let diagnostics = reload.diagnostics();
    // A refused document starts on the built-ins, but keeps naming its file:
    // the user fixes it and sends `SIGHUP` rather than restarting the daemon.
    let config = match reload {
        Reload::Applied { .. } => manager.config().clone(),
        Reload::Rejected { .. } | Reload::Detached => Config::defaults(),
    };
    InitialConfig {
        config,
        file: manager.file().cloned(),
        diagnostics,
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
