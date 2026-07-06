#![allow(dead_code)]

//! Application shell (PRD §8.8): the thin wiring that binds every module into a
//! running program. `main` resolves configuration, builds the AWS client,
//! discovers the table schema, then drives the TUI event loop. On *Start scan*
//! it assembles the [`RuleSet`], opens the export writers and fans a parallel
//! scan out through the [`crate::scan`] driver, piping each item through the
//! [`crate::rules`] engine into the export writers and the state aggregator.
//!
//! No business logic lives here: every decision is delegated to an owning
//! module. The shell only sequences them and moves data between them.

mod assemble;
mod aws;
mod config;
mod domain;
mod export;
mod rules;
mod scan;
mod state;
mod tui;

use std::fmt;
use std::fs::File;
use std::future::pending;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

use crate::assemble::{AssembleError, assemble};
use crate::aws::{AwsError, DynamoClient, RealDynamoClient, TableDescription, TableKeySchema};
use crate::config::{CliArgs, ConfigError, ExportConfig, ScanConfig};
use crate::domain::{AttributeValue, Item, KeyAttribute};
use crate::export::{CsvWriter, ExportError, ExportWriter, FanOutWriter, NdjsonWriter};
use crate::rules::{ItemViolations, RuleSet, Violation, check_item};
use crate::scan::{ScanStream, ScannedItem, run_scan};
use crate::state::{Aggregator, StateSnapshot};
use crate::tui::{App, Command, SetupScreen};

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

/// The mutable in-flight scan state, present only while a scan runs or its
/// results are being browsed.
struct ActiveScan {
    stream: Option<ScanStream>,
    writer: Option<Box<dyn ExportWriter>>,
    aggregator: Aggregator,
    context: ScanContext,
    export_paths: Vec<PathBuf>,
    consumed_pushed: f64,
    last_snapshot: StateSnapshot,
}

/// The resolved inputs the rule engine needs for one scan.
struct ScanContext {
    rules: RuleSet,
    table_key: TableKeySchema,
    detected_at: i64,
}

impl Shell {
    /// The TUI event loop (PRD §6.3.1). A single task interleaves terminal input,
    /// scanned-item processing and periodic redraws via `select!`, so the export
    /// writers never need to cross a task boundary. Redraws are driven by the
    /// frame ticker rather than per item, keeping a fast scan from starving the
    /// input handler.
    async fn event_loop(mut self, terminal: &mut DefaultTerminal) -> Result<(), ShellError> {
        let mut app = App::new(SetupScreen::new(&self.config, &self.description));
        let mut modal: Option<ErrorModal> = None;
        let mut scan: Option<ActiveScan> = None;
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

    /// Apply one shell-level [`Command`] from the TUI (PRD §8.6).
    async fn dispatch(
        &mut self,
        command: Command,
        app: &mut App,
        scan: &mut Option<ActiveScan>,
        modal: &mut Option<ErrorModal>,
        should_quit: &mut bool,
    ) {
        match command {
            Command::StartScan => match self.start_scan(app).await {
                Ok(active) => {
                    *scan = Some(active);
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
                if let Some(active) = scan
                    && let Some(stream) = &active.stream
                {
                    stream.cancel();
                }
            }
            Command::Quit => *should_quit = true,
        }
    }

    /// Build the scan pipeline for the setup screen's current form (PRD §8.8):
    /// resolve the config, re-discover the table if its name changed, assemble
    /// the [`RuleSet`], open the export writers and fan out the scan.
    async fn start_scan(&mut self, app: &App) -> Result<ActiveScan, ErrorModal> {
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

        let rules = assemble(&self.description, &config)?;
        config::resolve_export_paths(&mut config, &timestamp());
        let (writer, export_paths) = build_writer(&config.export)?;

        let aggregator =
            Aggregator::with_system_clock(config.segments as u32, self.description.item_count);
        let last_snapshot = aggregator.snapshot();
        let stream = run_scan(
            &config,
            Arc::clone(&self.client),
            self.description.provisioned_rcu,
        );

        self.config = config;

        Ok(ActiveScan {
            stream: Some(stream),
            writer: Some(writer),
            aggregator,
            context: ScanContext {
                rules,
                table_key: self.description.key_schema.clone(),
                detected_at: now_epoch_secs(),
            },
            export_paths,
            consumed_pushed: 0.0,
            last_snapshot,
        })
    }

    /// Persist the setup form to the resolved config path (PRD §6.3.3 Save).
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

    /// Fold one scanned item into the aggregator and export writer (PRD §6.2.1).
    fn consume(
        &self,
        item: ScannedItem,
        scan: &mut Option<ActiveScan>,
        modal: &mut Option<ErrorModal>,
    ) {
        let Some(active) = scan else { return };
        if let Err(err) = process_item(
            &item,
            &active.context,
            &active.aggregator,
            &mut active.writer,
        ) {
            modal.get_or_insert_with(|| ErrorModal::from(err));
        }
    }

    /// Close the export writers and move the app to the completed screen once
    /// every segment has terminated (PRD §6.3.5).
    fn finish_scan(
        &self,
        app: &mut App,
        scan: &mut Option<ActiveScan>,
        modal: &mut Option<ErrorModal>,
    ) {
        let Some(active) = scan else { return };
        active.stream = None;
        if let Some(writer) = active.writer.take()
            && let Err(err) = writer.close()
        {
            modal.get_or_insert_with(|| ErrorModal::from(err));
        }

        active.last_snapshot = active.aggregator.snapshot();
        app.complete(active.last_snapshot.recent_violations.len());
    }

    /// Render one frame: the current screen, the live snapshot while a scan is
    /// running, and any terminal-error modal on top (PRD §6.3.7).
    fn draw(
        &self,
        terminal: &mut DefaultTerminal,
        app: &App,
        scan: Option<&mut ActiveScan>,
        modal: Option<&ErrorModal>,
    ) -> Result<(), ShellError> {
        let (snapshot, paths) = match scan {
            Some(active) => {
                if let Some(stream) = &active.stream {
                    let total = stream.consumed_rcu();
                    let delta = total - active.consumed_pushed;
                    if delta != 0.0 {
                        active.aggregator.record_consumed(0, delta);
                        active.consumed_pushed = total;
                    }
                }

                (
                    Some(active.aggregator.snapshot()),
                    active.export_paths.clone(),
                )
            }
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

/// Evaluate one scanned item and stream any violations to disk and the
/// aggregator (PRD §8.1 / §8.5). Shared by the event loop and the integration
/// test so both exercise the identical item pipeline.
fn process_item(
    scanned: &ScannedItem,
    context: &ScanContext,
    aggregator: &Aggregator,
    writer: &mut Option<Box<dyn ExportWriter>>,
) -> Result<(), ExportError> {
    aggregator.record_item(scanned.segment);
    let violations = check_item(&scanned.item, &context.rules, context.detected_at);
    if violations.is_empty() {
        return Ok(());
    }

    for violation in &violations {
        aggregator.record_violation(violation);
    }

    if let Some(writer) = writer {
        let group = build_group(
            &context.rules.table,
            &context.table_key,
            scanned.item.clone(),
            violations,
            context.detected_at,
        );
        writer.write(&group)?;
    }

    Ok(())
}

/// Group an item's violations for export, extracting the table's own primary key
/// (PRD §6.6) so the detail view can later re-fetch the item by key.
fn build_group(
    table: &str,
    table_key: &TableKeySchema,
    item: Item,
    violations: Vec<Violation>,
    detected_at: i64,
) -> ItemViolations {
    let pk = key_attribute(&table_key.pk.name, &item);
    let sk = table_key.sk.as_ref().and_then(|element| {
        item.get(&element.name).map(|value| KeyAttribute {
            name: element.name.clone(),
            value: value.clone(),
        })
    });

    ItemViolations {
        table: table.to_string(),
        pk,
        sk,
        item,
        violations,
        detected_at,
    }
}

/// The named key attribute of an item. A scanned item always carries the table's
/// primary key; a null placeholder guards the impossible absent case rather than
/// panicking mid-scan.
fn key_attribute(name: &str, item: &Item) -> KeyAttribute {
    KeyAttribute {
        name: name.to_string(),
        value: item
            .get(name)
            .cloned()
            .unwrap_or(AttributeValue::Null(true)),
    }
}

/// Await `stream.next()` when a scan is running, otherwise never resolve, so the
/// idle branch stays inert in `select!` without busy-looping.
async fn next_scanned(scan: &mut Option<ActiveScan>) -> Option<Result<ScannedItem, AwsError>> {
    match scan.as_mut().and_then(|active| active.stream.as_mut()) {
        Some(stream) => stream.next().await,
        None => pending().await,
    }
}

/// Open the configured export writers (PRD §6.6). Paths are already resolved to
/// their defaults by [`config::resolve_export_paths`]; each enabled format is
/// created and reported so the completed screen can show where results landed.
fn build_writer(
    export: &ExportConfig,
) -> Result<(Box<dyn ExportWriter>, Vec<PathBuf>), ErrorModal> {
    let mut writers: Vec<Box<dyn ExportWriter>> = Vec::new();
    let mut paths = Vec::new();

    if export.csv
        && let Some(path) = &export.csv_path
    {
        let file = create_file(path)?;
        let writer = CsvWriter::new(BufWriter::new(file)).map_err(ErrorModal::from)?;
        writers.push(Box::new(writer));
        paths.push(path.clone());
    }

    if export.ndjson
        && let Some(path) = &export.ndjson_path
    {
        let file = create_file(path)?;
        writers.push(Box::new(NdjsonWriter::new(BufWriter::new(file))));
        paths.push(path.clone());
    }

    Ok((Box::new(FanOutWriter::new(writers)), paths))
}

fn create_file(path: &Path) -> Result<File, ErrorModal> {
    File::create(path).map_err(|source| {
        ErrorModal::message(
            "Cannot open export file",
            &format!("{}: {source}", path.display()),
        )
    })
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

/// Current wall-clock time as Unix epoch seconds, used to stamp violations and
/// as the TTL "now" (PRD §6.1.3).
fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A filesystem-safe timestamp for default export filenames (PRD §6.6). Epoch
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

/// A terminal-error modal (PRD §6.3.7): headline, optional SDK code, message and
/// a suggested remediation, dismissed by any keypress.
struct ErrorModal {
    title: String,
    code: Option<String>,
    message: String,
    remediation: Option<String>,
}

impl ErrorModal {
    fn message(title: &str, message: &str) -> Self {
        Self {
            title: title.to_string(),
            code: None,
            message: message.to_string(),
            remediation: None,
        }
    }

    fn render(&self, frame: &mut ratatui::Frame) {
        use ratatui::layout::Alignment;
        use ratatui::style::{Color, Modifier, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

        let area = tui::centered(frame.area(), 60, 12);
        frame.render_widget(Clear, area);

        let mut lines = Vec::new();
        if let Some(code) = &self.code {
            lines.push(Line::from(Span::styled(
                code.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }

        lines.push(Line::from(self.message.clone()));
        if let Some(hint) = &self.remediation {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                hint.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press any key to dismiss",
            Style::default().add_modifier(Modifier::DIM),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title))
            .border_style(Style::default().fg(Color::Red));

        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

impl From<AwsError> for ErrorModal {
    fn from(err: AwsError) -> Self {
        Self {
            title: "AWS error".to_string(),
            code: Some(err.code.clone()),
            message: err.message.clone(),
            remediation: err.remediation().map(str::to_string),
        }
    }
}

impl From<AssembleError> for ErrorModal {
    fn from(err: AssembleError) -> Self {
        Self::message("Cannot assemble rules", &err.to_string())
    }
}

impl From<ExportError> for ErrorModal {
    fn from(err: ExportError) -> Self {
        Self::message("Export failure", &err.to_string())
    }
}

impl From<ConfigError> for ErrorModal {
    fn from(err: ConfigError) -> Self {
        Self::message("Cannot save config", &err.to_string())
    }
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
    use crate::aws::mock::MockDynamoClient;
    use crate::aws::{IndexSchema, ScanResponse, TtlDescription};
    use crate::config::{GsiEntry, LsiEntry, TtlSettings};
    use crate::domain::{KeySchemaElement, TypeCode};
    use crate::rules::ViolationCategory;
    use std::io::Write;
    use std::sync::Mutex;

    /// A `Write` sink over a shared buffer, so a test can read exactly what an
    /// export writer produced after the writer is closed.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            SharedBuf(Arc::new(Mutex::new(Vec::new())))
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn s(value: &str) -> AttributeValue {
        AttributeValue::S(value.to_string())
    }

    fn item(pairs: &[(&str, AttributeValue)]) -> Item {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn description() -> TableDescription {
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
            ttl: Some(TtlDescription {
                attribute: "expiresAt".to_string(),
                enabled: true,
            }),
            provisioned_rcu: None,
            item_count: 3,
        }
    }

    fn config() -> ScanConfig {
        ScanConfig {
            table: "users".to_string(),
            region: None,
            profile: None,
            segments: 1,
            rate_limit_percent: None,
            export: ExportConfig {
                csv: true,
                csv_path: None,
                ndjson: true,
                ndjson_path: None,
            },
            gsi: vec![GsiEntry {
                name: "byEmail".to_string(),
                hypothetical: false,
                pk: None,
                sk: None,
                check_missing: true,
            }],
            lsi: Vec::<LsiEntry>::new(),
            ttl: Some(TtlSettings {
                enabled: Some(true),
                ..TtlSettings::default()
            }),
        }
    }

    fn page(items: Vec<Item>, next: Option<Item>) -> ScanResponse {
        ScanResponse {
            items,
            last_evaluated_key: next,
            consumed_rcu: Some(1.0),
        }
    }

    async fn drain(
        mut stream: ScanStream,
        context: &ScanContext,
        aggregator: &Aggregator,
        writer: &mut Option<Box<dyn ExportWriter>>,
    ) {
        while let Some(next) = stream.next().await {
            if let Ok(scanned) = next {
                process_item(&scanned, context, aggregator, writer).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn end_to_end_scan_detects_and_exports_violations() {
        let good = item(&[
            ("id", s("u1")),
            ("email", s("a@example.com")),
            ("expiresAt", AttributeValue::N("1700000000".to_string())),
        ]);
        // Missing the `email` GSI key and a wrong-typed TTL attribute.
        let bad = item(&[("id", s("u2")), ("expiresAt", s("not-a-number"))]);

        let client = Arc::new(
            MockDynamoClient::new()
                .with_describe("users", description())
                .with_scan_pages(0, [Ok(page(vec![good, bad], None))]),
        );

        let config = config();
        let rules = assemble(&description(), &config).unwrap();
        let aggregator = Aggregator::with_system_clock(1, 3);
        let context = ScanContext {
            rules,
            table_key: description().key_schema,
            detected_at: 1_700_000_000,
        };

        let csv = SharedBuf::new();
        let ndjson = SharedBuf::new();
        let writers: Vec<Box<dyn ExportWriter>> = vec![
            Box::new(CsvWriter::new(csv.clone()).unwrap()),
            Box::new(NdjsonWriter::new(ndjson.clone())),
        ];
        let mut writer: Option<Box<dyn ExportWriter>> = Some(Box::new(FanOutWriter::new(writers)));

        let stream = run_scan(&config, Arc::clone(&client) as _, None);
        drain(stream, &context, &aggregator, &mut writer).await;
        writer.take().unwrap().close().unwrap();

        let snapshot = aggregator.snapshot();
        assert_eq!(snapshot.items_scanned, 2);
        assert_eq!(snapshot.total_violations, 2);
        assert_eq!(
            snapshot.category_counts.get(&ViolationCategory::MissingKey),
            Some(&1)
        );
        assert_eq!(
            snapshot
                .category_counts
                .get(&ViolationCategory::TtlWrongType),
            Some(&1)
        );

        let csv = csv.contents();
        assert!(
            csv.lines()
                .next()
                .unwrap()
                .starts_with("table,target,category")
        );
        assert!(csv.contains("byEmail"));
        assert!(csv.contains("u2"));

        let ndjson = ndjson.contents();
        assert_eq!(
            ndjson.lines().count(),
            1,
            "only the violating item is written"
        );
        assert!(ndjson.contains("\"u2\""));
    }

    #[test]
    fn build_group_extracts_table_key() {
        let table_key = TableKeySchema {
            pk: element("id", TypeCode::S),
            sk: Some(element("ts", TypeCode::N)),
        };
        let it = item(&[("id", s("u9")), ("ts", AttributeValue::N("42".to_string()))]);
        let group = build_group("users", &table_key, it, Vec::new(), 100);

        assert_eq!(group.pk.name, "id");
        assert_eq!(group.pk.value, s("u9"));
        assert_eq!(group.sk.unwrap().value, AttributeValue::N("42".to_string()));
        assert_eq!(group.detected_at, 100);
    }

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
