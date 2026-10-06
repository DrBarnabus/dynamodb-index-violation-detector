//! Application shell: the thin wiring that binds every module into a running
//! program. The event loop starts on the profile picker or the setup screen;
//! once a profile is known it connects, lists tables and describes the chosen
//! one. *Estimate cost* re-describes the table to size the scan. On *Start
//! scan* it starts a scan `Pipeline` and feeds its items through it,
//! interleaved with terminal input and redraws. Once the scan completes,
//! drilling into a violation re-fetches its item.
//!
//! No business logic lives here: every decision is delegated to an owning
//! module. The shell only sequences them and moves data between them. Input
//! events, the draw target and the AWS connection are injected, so the loop
//! runs the same against a real terminal and a test harness.

use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ratatui::crossterm::event::Event;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::aws::{AwsError, DynamoClient, GetItemRequest, RealDynamoClient, TableDescription};
use crate::config::{self, ScanConfig};
use crate::estimate;
use crate::inspect::{self, Inspection};
use crate::pipeline::{Pipeline, now_epoch_secs};
use crate::profiles::Profile;
use crate::scan::ScannedItem;
use crate::state::RecentViolation;
use crate::tui::{App, Command, ErrorModal, ProfilePicker, SetupScreen, copy_to_clipboard};

/// Redraw cadence for the event loop (~20 fps): fast enough for live stats,
/// cheap enough that a high-throughput scan is not throttled by rendering.
const FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// Builds the DynamoDB client for a profile and region.
#[async_trait]
pub trait Connector {
    async fn connect(&self, profile: Option<&str>, region: Option<&str>) -> Arc<dyn DynamoClient>;
}

/// Connects to AWS with the SDK's credential and region resolution.
pub struct AwsConnector;

#[async_trait]
impl Connector for AwsConnector {
    async fn connect(&self, profile: Option<&str>, region: Option<&str>) -> Arc<dyn DynamoClient> {
        Arc::new(RealDynamoClient::new(profile, region).await)
    }
}

/// Where the event loop renders its frames.
pub trait DrawTarget {
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()>;
}

impl DrawTarget for DefaultTerminal {
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        Self::draw(self, render).map(|_| ())
    }
}

/// The state the event loop carries between screens. The client exists once a
/// profile is settled; the description is the most recently described table.
pub struct Shell {
    connector: Box<dyn Connector>,
    client: Option<Arc<dyn DynamoClient>>,
    config: ScanConfig,
    description: Option<TableDescription>,
    save_path: PathBuf,
}

impl Shell {
    /// A shell for `config` that persists the setup form to `save_path` and
    /// connects to DynamoDB through `connector`.
    pub fn new(config: ScanConfig, save_path: PathBuf, connector: Box<dyn Connector>) -> Self {
        Self {
            connector,
            client: None,
            config,
            description: None,
            save_path,
        }
    }

    /// The TUI event loop. A single task interleaves terminal input,
    /// scanned-item processing and periodic redraws via `select!`, so the export
    /// writers never need to cross a task boundary. Redraws are driven by the
    /// frame ticker rather than per item, keeping a fast scan from starving the
    /// input handler. Runs until the user quits or `input` closes.
    pub async fn event_loop(
        mut self,
        target: &mut impl DrawTarget,
        mut input: mpsc::Receiver<Event>,
        profiles: Vec<Profile>,
    ) -> io::Result<()> {
        let mut modal: Option<ErrorModal> = None;
        let mut app = if profiles.is_empty() {
            let (setup, error) = self.open_setup().await;
            modal = error;
            App::new(setup)
        } else {
            App::pick_profile(ProfilePicker::new(profiles))
        };
        let mut scan: Option<Pipeline> = None;
        let mut should_quit = false;

        let mut ticker = tokio::time::interval(FRAME_INTERVAL);

        self.draw(target, &app, scan.as_mut(), modal.as_ref())?;

        while !should_quit {
            tokio::select! {
                event = input.recv() => {
                    let Some(event) = event else { break };
                    if let Event::Key(key) = event {
                        if modal.take().is_some() {
                            // The dismissing keypress is swallowed, like the help overlay.
                        } else if let Some(command) = app.handle_key(key) {
                            self.dispatch(command, &mut app, &mut scan, &mut modal, &mut should_quit)
                                .await;
                        }
                    }

                    self.draw(target, &app, scan.as_mut(), modal.as_ref())?;
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
                    self.draw(target, &app, scan.as_mut(), modal.as_ref())?;
                }
            }
        }

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
            Command::SelectProfile(profile) => {
                self.config.profile = Some(profile.name);
                if self.config.region.is_none() {
                    self.config.region = profile.region;
                }

                let (setup, error) = self.open_setup().await;
                app.show_setup(setup);
                *modal = error;
            }
            Command::ChangeRegion(region) => {
                *modal = self.change_region(region, app).await;
            }
            Command::SelectTable(name) => {
                if let Err(err) = self.select_table(&name, app).await {
                    *modal = Some(err);
                }
            }
            Command::EstimateCost => {
                if let Err(err) = self.estimate_cost(app).await {
                    *modal = Some(err);
                }
            }
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
            Command::DrillInto(recent) => match self.drill_into(&recent, scan.as_ref()).await {
                Ok(inspection) => app.show_inspection(inspection),
                Err(err) => *modal = Some(err),
            },
            Command::CopyToClipboard(text) => {
                if let Err(err) = copy_to_clipboard(&text) {
                    *modal = Some(ErrorModal::message(
                        "Could not copy to the clipboard",
                        &format!("writing to the terminal failed: {err}"),
                    ));
                }
            }
            Command::Quit => *should_quit = true,
        }
    }

    /// Build the client for the settled profile, list its tables and describe
    /// the configured table, if any. A failure is returned as a modal over the
    /// setup screen, which still opens so the user can type a table or quit.
    async fn open_setup(&mut self) -> (SetupScreen, Option<ErrorModal>) {
        let table = self.config.table.clone();
        let (tables, error) = self.connect(&table).await;
        let setup = SetupScreen::new(&self.config, self.description.as_ref(), tables);
        (setup, error)
    }

    /// Reconnect in `region`, then refresh the setup screen's table list and
    /// re-describe its loaded table there. A table absent from the new region
    /// leaves the form without discovered rows, and the error explains why.
    async fn change_region(&mut self, region: Option<String>, app: &mut App) -> Option<ErrorModal> {
        let setup = app.setup_mut()?;
        self.config.region = region;

        let table = setup.loaded_table().unwrap_or_default().to_string();
        let (tables, error) = self.connect(&table).await;
        setup.set_tables(tables);
        match &self.description {
            Some(description) => setup.load_table(description),
            None => setup.unload_table(),
        }

        error
    }

    /// Build the client for the current profile and region, then list tables
    /// and describe `table` (when non-empty) concurrently. The description
    /// replaces the stored one; the first failure is returned as a modal.
    async fn connect(&mut self, table: &str) -> (Vec<String>, Option<ErrorModal>) {
        let client = self
            .connector
            .connect(
                self.config.profile.as_deref(),
                self.config.region.as_deref(),
            )
            .await;
        self.client = Some(Arc::clone(&client));

        let (tables, description) = tokio::join!(client.list_tables(), async {
            if table.is_empty() {
                Ok(None)
            } else {
                client.describe_table(table).await.map(Some)
            }
        });

        let mut error = None;
        let tables = tables.unwrap_or_else(|err| {
            error = Some(ErrorModal::from(err));
            Vec::new()
        });
        self.description = description.unwrap_or_else(|err| {
            error.get_or_insert_with(|| ErrorModal::from(err));
            None
        });

        (tables, error)
    }

    /// Describe the table chosen in the setup picker and rebuild the form's
    /// index rows from it.
    async fn select_table(&mut self, name: &str, app: &mut App) -> Result<(), ErrorModal> {
        let client = self.client()?;
        let description = client
            .describe_table(name)
            .await
            .map_err(ErrorModal::from)?;
        if let Some(setup) = app.setup_mut() {
            setup.load_table(&description);
        }

        self.description = Some(description);
        Ok(())
    }

    /// Describe the form's table afresh, for an up-to-date size and capacity,
    /// and show the estimated cost of scanning it with the form's settings.
    async fn estimate_cost(&mut self, app: &mut App) -> Result<(), ErrorModal> {
        let setup = app.setup_mut().ok_or_else(|| {
            ErrorModal::message("Cannot estimate cost", "no setup screen is active")
        })?;
        let config = setup
            .to_scan_config()
            .map_err(|message| ErrorModal::message("Invalid scan settings", &message))?;

        let client = self.client()?;
        let description = client
            .describe_table(&config.table)
            .await
            .map_err(ErrorModal::from)?;
        if setup.loaded_table() != Some(description.name.as_str()) {
            setup.load_table(&description);
        }

        setup.show_estimate(estimate::estimate(
            &description,
            config.segments,
            config.rate_limit_percent,
        ));
        self.description = Some(description);
        Ok(())
    }

    fn client(&self) -> Result<Arc<dyn DynamoClient>, ErrorModal> {
        self.client.clone().ok_or_else(|| {
            ErrorModal::message(
                "No AWS client",
                "choose an AWS profile before using the setup screen",
            )
        })
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

        let client = self.client()?;
        let description = match &mut self.description {
            Some(description) if description.name == config.table => description,
            slot => slot.insert(
                client
                    .describe_table(&config.table)
                    .await
                    .map_err(ErrorModal::from)?,
            ),
        };

        config::resolve_export_paths(&mut config, &timestamp());
        let pipeline = Pipeline::start(description, &config, client)?;
        self.config = config;
        Ok(pipeline)
    }

    /// Re-fetch a violation's item by key and compare it with what the scan saw.
    async fn drill_into(
        &self,
        recent: &RecentViolation,
        scan: Option<&Pipeline>,
    ) -> Result<Inspection, ErrorModal> {
        let pipeline = scan.ok_or_else(|| {
            ErrorModal::message("Cannot inspect violation", "no scan has been run")
        })?;
        let rules = pipeline.rules();
        let request = GetItemRequest {
            table: rules.table.clone(),
            key: inspect::primary_key(&recent.pk, recent.sk.as_ref()),
        };
        let current = self
            .client()?
            .get_item(request)
            .await
            .map_err(ErrorModal::from)?;

        Ok(inspect::inspect(recent, current, rules, now_epoch_secs()))
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

        app.complete(pipeline.snapshot().recent_violations);
    }

    /// Render one frame: the current screen, the live snapshot while a scan is
    /// running, and any terminal-error modal on top.
    fn draw(
        &self,
        target: &mut impl DrawTarget,
        app: &App,
        scan: Option<&mut Pipeline>,
        modal: Option<&ErrorModal>,
    ) -> io::Result<()> {
        let (snapshot, paths) = match scan {
            Some(pipeline) => (Some(pipeline.snapshot()), pipeline.export_paths().to_vec()),
            None => (None, Vec::new()),
        };

        target.draw(|frame| {
            app.render(frame, snapshot.as_ref(), &paths);
            if let Some(modal) = modal {
                modal.render(frame);
            }
        })
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

/// A filesystem-safe timestamp for default export filenames. Epoch
/// seconds avoid a calendar-formatting dependency while staying unique per scan.
fn timestamp() -> String {
    now_epoch_secs().to_string()
}
