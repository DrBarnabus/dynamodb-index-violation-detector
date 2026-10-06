//! Entry point: resolves configuration and AWS profiles, then hands the
//! terminal to the [`Shell`] event loop. Failures before the TUI starts are
//! reported on stderr.

use std::env;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use dynamodb_violation_detector::config::{self, CliArgs, ConfigError};
use dynamodb_violation_detector::profiles::{self, ProfileError};
use dynamodb_violation_detector::shell::{AwsConnector, Shell};
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// The default config file consulted when `--config` is not given.
const DEFAULT_CONFIG_FILE: &str = "scan.toml";

#[tokio::main]
async fn main() -> ExitCode {
    let cli = CliArgs::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve configuration and discover profiles, then run the TUI. Failures
/// before the TUI starts surface on stderr (see [`main`]); once the TUI owns the
/// terminal, errors are shown as a modal instead.
async fn run(cli: CliArgs) -> Result<(), ShellError> {
    let config_path = resolve_config_path(&cli);
    let config = config::load(config_path.as_deref(), &cli)?;

    let env_profile = env::var_os("AWS_PROFILE").is_some();
    let profiles = if needs_profile_picker(config_path.is_some(), env_profile, &cli) {
        profiles::discover()?
    } else {
        Vec::new()
    };

    let save_path = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE));
    let shell = Shell::new(config, save_path, Box::new(AwsConnector));

    let mut terminal = ratatui::try_init().map_err(ShellError::Io)?;
    let (input, input_shutdown) = spawn_input_reader();
    let result = shell.event_loop(&mut terminal, input, profiles).await;
    input_shutdown.store(true, Ordering::Relaxed);
    ratatui::restore();
    result.map_err(ShellError::Io)
}

/// Which config file to read: an explicit `--config`, else `./scan.toml` when it
/// exists, else none (CLI + built-in defaults only).
fn resolve_config_path(cli: &CliArgs) -> Option<PathBuf> {
    if let Some(path) = &cli.config {
        return Some(path.clone());
    }

    let default = PathBuf::from(DEFAULT_CONFIG_FILE);
    default.exists().then_some(default)
}

/// The profile picker runs only when nothing already targets the scan: no
/// config file, no `AWS_PROFILE`, and no CLI override.
fn needs_profile_picker(has_config_file: bool, env_profile: bool, cli: &CliArgs) -> bool {
    !has_config_file
        && !env_profile
        && cli.table.is_none()
        && cli.profile.is_none()
        && cli.region.is_none()
        && cli.segments.is_none()
        && cli.rate_limit_percent.is_none()
}

/// Spawn the blocking terminal-input reader on a dedicated thread, forwarding
/// events over a channel. The thread polls with a short timeout so it observes
/// the shutdown flag promptly once the event loop ends.
fn spawn_input_reader() -> (mpsc::Receiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel(64);
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if tx.blocking_send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    (rx, shutdown)
}

/// A failure that occurs before the TUI takes over the terminal.
#[derive(Debug)]
enum ShellError {
    Config(ConfigError),
    Profiles(ProfileError),
    Io(io::Error),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Config(err) => write!(f, "{err}"),
            ShellError::Profiles(err) => write!(f, "{err}"),
            ShellError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShellError::Config(err) => Some(err),
            ShellError::Profiles(err) => Some(err),
            ShellError::Io(err) => Some(err),
        }
    }
}

impl From<ConfigError> for ShellError {
    fn from(err: ConfigError) -> Self {
        ShellError::Config(err)
    }
}

impl From<ProfileError> for ShellError {
    fn from(err: ProfileError) -> Self {
        ShellError::Profiles(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_path_prefers_cli_over_default() {
        let cli = CliArgs {
            config: Some(PathBuf::from("/tmp/explicit.toml")),
            ..CliArgs::default()
        };
        assert_eq!(
            resolve_config_path(&cli),
            Some(PathBuf::from("/tmp/explicit.toml"))
        );
    }

    #[test]
    fn profile_picker_runs_only_when_nothing_targets_the_scan() {
        assert!(needs_profile_picker(false, false, &CliArgs::default()));
        assert!(!needs_profile_picker(true, false, &CliArgs::default()));
        assert!(!needs_profile_picker(false, true, &CliArgs::default()));

        let overrides = [
            CliArgs {
                table: Some("t".to_string()),
                ..CliArgs::default()
            },
            CliArgs {
                profile: Some("p".to_string()),
                ..CliArgs::default()
            },
            CliArgs {
                region: Some("r".to_string()),
                ..CliArgs::default()
            },
            CliArgs {
                segments: Some(2),
                ..CliArgs::default()
            },
            CliArgs {
                rate_limit_percent: Some(50),
                ..CliArgs::default()
            },
        ];
        for cli in overrides {
            assert!(!needs_profile_picker(false, false, &cli), "{cli:?}");
        }
    }
}
