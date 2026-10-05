//! Application shell: the thin wiring that binds every module into a
//! running program. `main` resolves configuration, builds the AWS client,
//! discovers the table schema, then drives the TUI event loop. On *Start scan*
//! it starts a scan `Pipeline` and feeds its items through it, interleaved with
//! terminal input and redraws.
//!
//! No business logic lives here: every decision is delegated to an owning
//! module. The shell only sequences them and moves data between them.

use std::fmt;
use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use dynamodb_violation_detector::aws::{
    AwsError, DynamoClient, RealDynamoClient, TableDescription,
};
use dynamodb_violation_detector::config::{self, CliArgs, ConfigError, ScanConfig};
use dynamodb_violation_detector::pipeline::{Pipeline, now_epoch_secs};
use dynamodb_violation_detector::scan::ScannedItem;
use dynamodb_violation_detector::tui::{App, Command, ErrorModal, SetupScreen};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// The default config file consulted when `--config` is not given.
const DEFAULT_CONFIG_FILE: &str = "scan.toml";

/// Redraw cadence for the event loop (~20 fps): fast enough for live stats,
/// cheap enough that a high-throughput scan is not throttled by rendering.
const FRAME_INTERVAL: Duration = Duration::from_millis(50);

#[tokio::main]
async fn main() -> ExitCode {
    let cli = CliArgs::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            if let Some(hint) = err.remediation() {
                eprintln!("  {hint}");
            }

            ExitCode::FAILURE
        }
    }
}

/// Resolve configuration, build the client and discover the table, then run the
/// TUI. Failures before the TUI starts surface on stderr (see [`main`]); once the
/// TUI owns the terminal, terminal errors are shown as a modal instead.
async fn run(cli: CliArgs) -> Result<(), ShellError> {
    let config_path = resolve_config_path(&cli);
    let config = config::load(config_path.as_deref(), &cli)?;

    let client: Arc<dyn DynamoClient> =
        Arc::new(RealDynamoClient::new(config.profile.as_deref(), config.region.as_deref()).await);

    let description = client.describe_table(&config.table).await?;

    let save_path = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE));
    let shell = Shell {
        client,
        config,
        description,
        save_path,
    };

    let mut terminal = ratatui::try_init().map_err(ShellError::Io)?;
    let result = shell.event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

/// Everything the event loop needs that is fixed for the program's lifetime.
struct Shell {
    client: Arc<dyn DynamoClient>,
    config: ScanConfig,
    description: TableDescription,
    save_path: PathBuf,
}

impl Shell {
    /// The TUI event loop. A single task interleaves terminal input,
    /// scanned-item processing and periodic redraws via `select!`, so the export
    /// writers never need to cross a task boundary. Redraws are driven by the
    /// frame ticker rather than per item, keeping a fast scan from starving the
    /// input handler.
    async fn event_loop(mut self, terminal: &mut DefaultTerminal) -> Result<(), ShellError> {
        let mut app = App::new(SetupScreen::new(&self.config, &self.description));
        let mut modal: Option<ErrorModal> = None;
        let mut scan: Option<Pipeline> = None;
        let mut should_quit = false;

        let (mut input_rx, input_shutdown) = spawn_input_reader();
        let mut ticker = tokio::time::interval(FRAME_INTERVAL);

        self.draw(terminal, &app, scan.as_mut(), modal.as_ref())?;

        while !should_quit {
            tokio::select! {
                event = input_rx.recv() => {
                    let Some(event) = event else { break };
                    if let Event::Key(key) = event {
                        if modal.take().is_some() {
                            // The dismissing keypress is swallowed, like the help overlay.
                        } else if let Some(command) = app.handle_key(key) {
                            self.dispatch(command, &mut app, &mut scan, &mut modal, &mut should_quit)
                                .await;
                        }
                    }

                    self.draw(terminal, &app, scan.as_mut(), modal.as_ref())?;
                }
                scanned = next_scanned(&mut scan) => {
                    match scanned {
                        Some(Ok(item)) => self.consume(item, &mut scan, &mut modal),
                        Some(Err(err)) => {
                            modal.get_or_insert_with(|| ErrorModal::from(err));
                        }
                        None => self.finish_scan(&mut app, &mut scan, &mut modal),
                    }
                }
                _ = ticker.tick() => {
                    self.draw(terminal, &app, scan.as_mut(), modal.as_ref())?;
                }
            }
        }

        input_shutdown.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Apply one shell-level [`Command`] from the TUI.
    async fn dispatch(
        &mut self,
        command: Command,
        app: &mut App,
        scan: &mut Option<Pipeline>,
        modal: &mut Option<ErrorModal>,
        should_quit: &mut bool,
    ) {
        match command {
            Command::StartScan => match self.start_scan(app).await {
                Ok(pipeline) => {
                    *scan = Some(pipeline);
                    app.begin_scan();
                }
                Err(err) => *modal = Some(err),
            },
            Command::SaveConfig => {
                if let Err(err) = self.save_config(app) {
                    *modal = Some(err);
                }
            }
            Command::CancelScan => {
                if let Some(pipeline) = scan {
                    pipeline.cancel();
                }
            }
            Command::Quit => *should_quit = true,
        }
    }

    /// Start a scan pipeline for the setup screen's current form: resolve the
    /// config and re-discover the table if its name changed.
    async fn start_scan(&mut self, app: &App) -> Result<Pipeline, ErrorModal> {
        let setup = app
            .setup()
            .ok_or_else(|| ErrorModal::message("Cannot start scan", "no setup screen is active"))?;

        let mut config = setup
            .to_scan_config()
            .map_err(|message| ErrorModal::message("Invalid scan settings", &message))?;
        config.profile = self.config.profile.clone();

        if config.table != self.description.name {
            self.description = self
                .client
                .describe_table(&config.table)
                .await
                .map_err(ErrorModal::from)?;
        }

        config::resolve_export_paths(&mut config, &timestamp());
        let pipeline = Pipeline::start(&self.description, &config, Arc::clone(&self.client))?;
        self.config = config;
        Ok(pipeline)
    }

    /// Persist the setup form to the resolved config path.
    fn save_config(&self, app: &App) -> Result<(), ErrorModal> {
        let setup = app.setup().ok_or_else(|| {
            ErrorModal::message("Cannot save config", "no setup screen is active")
        })?;
        let mut config = setup
            .to_scan_config()
            .map_err(|message| ErrorModal::message("Invalid scan settings", &message))?;
        config.profile = self.config.profile.clone();

        config::save(&config, &self.save_path).map_err(ErrorModal::from)
    }

    /// Fold one scanned item into the aggregator and export writer.
    fn consume(
        &self,
        item: ScannedItem,
        scan: &mut Option<Pipeline>,
        modal: &mut Option<ErrorModal>,
    ) {
        let Some(pipeline) = scan else { return };
        if let Err(err) = pipeline.process(item) {
            modal.get_or_insert_with(|| ErrorModal::from(err));
        }
    }

    /// Close the export writers and move the app to the completed screen once
    /// every segment has terminated.
    fn finish_scan(
        &self,
        app: &mut App,
        scan: &mut Option<Pipeline>,
        modal: &mut Option<ErrorModal>,
    ) {
        let Some(pipeline) = scan else { return };
        if let Err(err) = pipeline.finish() {
            modal.get_or_insert_with(|| ErrorModal::from(err));
        }

        app.complete(pipeline.snapshot().recent_violations.len());
    }

    /// Render one frame: the current screen, the live snapshot while a scan is
    /// running, and any terminal-error modal on top.
    fn draw(
        &self,
        terminal: &mut DefaultTerminal,
        app: &App,
        scan: Option<&mut Pipeline>,
        modal: Option<&ErrorModal>,
    ) -> Result<(), ShellError> {
        let (snapshot, paths) = match scan {
            Some(pipeline) => (Some(pipeline.snapshot()), pipeline.export_paths().to_vec()),
            None => (None, Vec::new()),
        };

        terminal
            .draw(|frame| {
                app.render(frame, snapshot.as_ref(), &paths);
                if let Some(modal) = modal {
                    modal.render(frame);
                }
            })
            .map_err(ShellError::Io)?;

        Ok(())
    }
}

/// Await `stream.next()` when a scan is running, otherwise never resolve, so the
/// idle branch stays inert in `select!` without busy-looping.
async fn next_scanned(scan: &mut Option<Pipeline>) -> Option<Result<ScannedItem, AwsError>> {
    match scan.as_mut().filter(|pipeline| pipeline.is_running()) {
        Some(pipeline) => pipeline.next().await,
        None => pending().await,
    }
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

/// A filesystem-safe timestamp for default export filenames. Epoch
/// seconds avoid a calendar-formatting dependency while staying unique per scan.
fn timestamp() -> String {
    now_epoch_secs().to_string()
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
    Aws(AwsError),
    Io(io::Error),
}

impl ShellError {
    fn remediation(&self) -> Option<&'static str> {
        match self {
            ShellError::Aws(err) => err.remediation(),
            _ => None,
        }
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Config(err) => write!(f, "{err}"),
            ShellError::Aws(err) => write!(f, "{err}"),
            ShellError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShellError::Config(err) => Some(err),
            ShellError::Aws(err) => Some(err),
            ShellError::Io(err) => Some(err),
        }
    }
}

impl From<ConfigError> for ShellError {
    fn from(err: ConfigError) -> Self {
        ShellError::Config(err)
    }
}

impl From<AwsError> for ShellError {
    fn from(err: AwsError) -> Self {
        ShellError::Aws(err)
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
}
