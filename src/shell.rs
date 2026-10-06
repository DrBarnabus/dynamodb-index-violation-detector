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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::watch;
    use tokio::time::timeout;

    use super::*;
    use crate::aws::mock::MockDynamoClient;
    use crate::aws::{IndexSchema, ScanResponse, TableKeySchema};
    use crate::config::ExportConfig;
    use crate::domain::{AttributeValue, Item, KeySchemaElement, TypeCode};
    use crate::tui::buffer_text;

    /// How long a scripted session may run before the test fails as hung.
    const SESSION_LIMIT: Duration = Duration::from_secs(10);

    type Connection = (Option<String>, Option<String>);

    /// Hands out one shared mock client and records each connection's profile
    /// and region.
    struct MockConnector {
        client: Arc<MockDynamoClient>,
        connections: Arc<Mutex<Vec<Connection>>>,
    }

    #[async_trait]
    impl Connector for MockConnector {
        async fn connect(
            &self,
            profile: Option<&str>,
            region: Option<&str>,
        ) -> Arc<dyn DynamoClient> {
            self.connections
                .lock()
                .unwrap()
                .push((profile.map(str::to_string), region.map(str::to_string)));
            Arc::clone(&self.client) as Arc<dyn DynamoClient>
        }
    }

    /// Renders into an in-memory terminal and publishes each frame's text.
    struct TestScreen {
        terminal: Terminal<TestBackend>,
        frames: watch::Sender<String>,
    }

    impl DrawTarget for TestScreen {
        fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
            let Ok(frame) = self.terminal.draw(render);
            self.frames.send_replace(buffer_text(frame.buffer));
            Ok(())
        }
    }

    /// The user's side of a session: keys go in, rendered frames come out.
    struct Ui {
        keys: mpsc::Sender<Event>,
        frames: watch::Receiver<String>,
    }

    impl Ui {
        async fn press(&self, code: KeyCode) {
            self.send(KeyEvent::new(code, KeyModifiers::NONE)).await;
        }

        async fn press_ctrl(&self, c: char) {
            self.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
                .await;
        }

        async fn type_text(&self, text: &str) {
            for c in text.chars() {
                self.press(KeyCode::Char(c)).await;
            }
        }

        async fn send(&self, key: KeyEvent) {
            self.keys
                .send(Event::Key(key))
                .await
                .expect("event loop stopped reading input");
        }

        async fn wait_for(&mut self, text: &str) {
            self.wait_until(text, |frame| frame.contains(text)).await;
        }

        async fn wait_for_absence(&mut self, text: &str) {
            self.wait_until(text, |frame| !frame.contains(text)).await;
        }

        async fn wait_until(&mut self, text: &str, condition: impl FnMut(&String) -> bool) {
            let settled = matches!(
                timeout(Duration::from_secs(5), self.frames.wait_for(condition)).await,
                Ok(Ok(_))
            );
            assert!(
                settled,
                "screen never settled on {text:?}; last frame:\n{}",
                *self.frames.borrow()
            );
        }
    }

    /// Run `shell` against `script`. The script returns its [`Ui`] so input
    /// stays open, so the session must end through the loop's own quit path.
    async fn run_session(
        shell: Shell,
        profiles: Vec<Profile>,
        script: impl AsyncFnOnce(Ui) -> Ui,
    ) -> io::Result<()> {
        let (keys, input) = mpsc::channel(64);
        let (frames_tx, frames) = watch::channel(String::new());
        let mut screen = TestScreen {
            terminal: Terminal::new(TestBackend::new(120, 50)).unwrap(),
            frames: frames_tx,
        };

        let session = async {
            tokio::join!(
                shell.event_loop(&mut screen, input, profiles),
                script(Ui { keys, frames })
            )
        };
        let (result, _ui) = timeout(SESSION_LIMIT, session)
            .await
            .expect("event loop did not quit");
        result
    }

    struct Fixture {
        client: Arc<MockDynamoClient>,
        connections: Arc<Mutex<Vec<Connection>>>,
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str, client: MockDynamoClient) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ddb-violation-detector-shell-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();

            Self {
                client: Arc::new(client),
                connections: Arc::default(),
                dir,
            }
        }

        fn shell(&self, config: ScanConfig) -> Shell {
            let connector = MockConnector {
                client: Arc::clone(&self.client),
                connections: Arc::clone(&self.connections),
            };
            Shell::new(config, self.save_path(), Box::new(connector))
        }

        fn save_path(&self) -> PathBuf {
            self.dir.join("scan.toml")
        }

        fn csv_path(&self) -> PathBuf {
            self.dir.join("violations.csv")
        }

        fn config(&self, table: &str) -> ScanConfig {
            ScanConfig {
                table: table.to_string(),
                region: Some("eu-west-1".to_string()),
                profile: None,
                segments: 1,
                rate_limit_percent: None,
                export: ExportConfig {
                    csv: true,
                    csv_path: Some(self.csv_path()),
                    ndjson: false,
                    ndjson_path: None,
                },
                gsi: Vec::new(),
                lsi: Vec::new(),
                ttl: None,
            }
        }

        fn connections(&self) -> Vec<Connection> {
            self.connections.lock().unwrap().clone()
        }
    }

    fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn users() -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: element("id", TypeCode::S),
                sk: None,
            },
            gsis: vec![IndexSchema {
                name: "byEmail".to_string(),
                pk: element("email", TypeCode::S),
                sk: None,
            }],
            lsis: Vec::new(),
            ttl: None,
            provisioned_rcu: None,
            item_count: 2,
            table_size_bytes: 0,
        }
    }

    fn user(id: &str, email: AttributeValue) -> Item {
        Item::from([
            ("id".to_string(), AttributeValue::S(id.to_string())),
            ("email".to_string(), email),
        ])
    }

    fn violating_user() -> Item {
        user("u-1", AttributeValue::N("5".to_string()))
    }

    /// One page holding a GSI type mismatch (`u-1`) and a clean item (`u-2`).
    fn scanned_users() -> MockDynamoClient {
        MockDynamoClient::new()
            .with_tables(["users"])
            .with_describe("users", users())
            .with_scan_pages(
                0,
                [Ok(ScanResponse {
                    items: vec![
                        violating_user(),
                        user("u-2", AttributeValue::S("a@example.com".to_string())),
                    ],
                    last_evaluated_key: None,
                    consumed_rcu: None,
                })],
            )
    }

    fn profile(name: &str, region: Option<&str>) -> Profile {
        Profile {
            name: name.to_string(),
            region: region.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn configured_table_opens_setup_with_its_schema() {
        let fixture = Fixture::new(
            "configured",
            MockDynamoClient::new()
                .with_tables(["orders", "users"])
                .with_describe("users", users()),
        );

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.wait_for("byEmail").await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            fixture.connections(),
            vec![(None, Some("eu-west-1".to_string()))]
        );
        assert_eq!(fixture.client.list_tables_call_count(), 1);
        assert_eq!(fixture.client.recorded_describes(), vec!["users"]);
    }

    #[tokio::test]
    async fn chosen_profile_connects_in_its_default_region() {
        let fixture = Fixture::new(
            "profile",
            MockDynamoClient::new()
                .with_tables(["users"])
                .with_describe("users", users()),
        );
        let mut config = fixture.config("users");
        config.region = None;
        let profiles = vec![profile("prod", None), profile("dev", Some("eu-west-2"))];

        let result = run_session(fixture.shell(config), profiles, async |mut ui| {
            ui.wait_for("Choose AWS profile").await;
            ui.type_text("dev").await;
            ui.press(KeyCode::Enter).await;
            ui.wait_for("Scan setup").await;
            ui.press(KeyCode::Esc).await;
            ui
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(
            fixture.connections(),
            vec![(Some("dev".to_string()), Some("eu-west-2".to_string()))]
        );
    }

    #[tokio::test]
    async fn aws_error_modal_swallows_the_dismissing_key() {
        let fixture = Fixture::new("modal", MockDynamoClient::new().with_tables(["users"]));

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("AWS error").await;
                ui.press(KeyCode::Esc).await;
                ui.wait_for_absence("AWS error").await;
                ui.wait_for("Scan setup").await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn choosing_a_table_describes_it() {
        let fixture = Fixture::new(
            "choose-table",
            MockDynamoClient::new()
                .with_tables(["orders", "users"])
                .with_describe("users", users()),
        );

        let result = run_session(
            fixture.shell(fixture.config("")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.type_text("users").await;
                ui.press(KeyCode::Enter).await;
                ui.wait_for("byEmail").await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(fixture.client.recorded_describes(), vec!["users"]);
    }

    #[tokio::test]
    async fn leaving_an_edited_region_reconnects_there() {
        let fixture = Fixture::new(
            "region",
            MockDynamoClient::new()
                .with_tables(["users"])
                .with_describe("users", users()),
        );

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.press(KeyCode::Tab).await;
                for _ in "eu-west-1".chars() {
                    ui.press(KeyCode::Backspace).await;
                }
                ui.type_text("us-east-1").await;
                ui.press(KeyCode::Tab).await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            fixture.connections(),
            vec![
                (None, Some("eu-west-1".to_string())),
                (None, Some("us-east-1".to_string())),
            ]
        );
        assert_eq!(fixture.client.list_tables_call_count(), 2);
        assert_eq!(fixture.client.recorded_describes(), vec!["users", "users"]);
    }

    #[tokio::test]
    async fn scan_runs_to_completion_and_exports_violations() {
        let fixture = Fixture::new("scan", scanned_users());

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.press(KeyCode::BackTab).await;
                ui.press(KeyCode::Enter).await;
                ui.wait_for("Scan complete").await;
                ui.wait_for("Recent violations (1)").await;
                ui.press(KeyCode::Char('q')).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(fixture.client.recorded_scans().len(), 1);
        let csv = fs::read_to_string(fixture.csv_path()).unwrap();
        assert!(csv.contains("u-1"), "{csv}");
        assert!(!csv.contains("u-2"), "{csv}");
    }

    #[tokio::test]
    async fn drilling_into_a_violation_refetches_its_item() {
        let fixture = Fixture::new(
            "drill",
            scanned_users().with_get_item(Ok(Some(violating_user()))),
        );

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.press(KeyCode::BackTab).await;
                ui.press(KeyCode::Enter).await;
                ui.wait_for("Scan complete").await;
                ui.press(KeyCode::Enter).await;
                ui.wait_for("Violation detail").await;
                ui.press(KeyCode::Esc).await;
                ui.wait_for_absence("Violation detail").await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            fixture.client.recorded_get_items(),
            vec![GetItemRequest {
                table: "users".to_string(),
                key: Item::from([("id".to_string(), AttributeValue::S("u-1".to_string()))]),
            }]
        );
    }

    #[tokio::test]
    async fn save_shortcut_writes_the_setup_form() {
        let fixture = Fixture::new(
            "save",
            MockDynamoClient::new()
                .with_tables(["users"])
                .with_describe("users", users()),
        );

        let result = run_session(
            fixture.shell(fixture.config("users")),
            Vec::new(),
            async |mut ui| {
                ui.wait_for("Scan setup").await;
                ui.press_ctrl('s').await;
                ui.press(KeyCode::Esc).await;
                ui
            },
        )
        .await;

        assert!(result.is_ok());
        let saved = config::load(Some(&fixture.save_path()), &Default::default()).unwrap();
        assert_eq!(saved.table, "users");
        assert_eq!(saved.region.as_deref(), Some("eu-west-1"));
    }

    #[tokio::test]
    async fn closed_input_ends_the_loop() {
        let fixture = Fixture::new(
            "closed",
            MockDynamoClient::new()
                .with_tables(["users"])
                .with_describe("users", users()),
        );
        let (keys, input) = mpsc::channel(1);
        let (frames, _) = watch::channel(String::new());
        let mut screen = TestScreen {
            terminal: Terminal::new(TestBackend::new(120, 50)).unwrap(),
            frames,
        };
        drop(keys);

        let shell = fixture.shell(fixture.config("users"));
        let result = timeout(
            SESSION_LIMIT,
            shell.event_loop(&mut screen, input, Vec::new()),
        )
        .await
        .expect("event loop kept running after input closed");

        assert!(result.is_ok());
    }
}
