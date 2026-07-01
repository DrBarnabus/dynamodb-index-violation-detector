# DynamoDB Index Violation Detector — Product Requirements Document

## 1. Overview

A Rust-based terminal tool for scanning a DynamoDB table to detect items that violate the key schema of a GSI (existing or hypothetical), the key schema of an LSI, or the expected shape of a TTL attribute. The tool reports violations in a TUI and streams them to CSV/NDJSON export files for downstream remediation.

It fills the gap left by the now-archived `awslabs/dynamodb-online-index-violation-detector` Java CLI, targeting experienced engineers who need to audit a table before adding a GSI or to investigate an existing index.

## 2. Motivation

When a GSI is created on a pre-existing DynamoDB table, DynamoDB backfills it by scanning existing items. Items whose proposed-key attributes are missing, of the wrong type, or exceed the index key size limits are **silently not indexed** — no error is raised at GSI creation time. New writes after GSI creation are validated against the key schema at write time and rejected if malformed, but historical data predating the index is the source of most real-world violations.

The primary motivating workflow: an engineer wants to add a new GSI to an existing table and needs to know, up front, which items will fail to be indexed and why, so they can remediate before the migration.

## 3. Goals

- Detect GSI violations against either an existing index (schema discovered via `DescribeTable`) or a hypothetical index spec (pre-flight)
- Detect LSI sparse-index violations (items missing the LSI sort key)
- Detect TTL attribute violations (missing, wrong type, millisecond-magnitude value, malformed)
- Provide an interactive TUI for driving scans and reviewing results
- Scale to large tables via parallel scan, with operator control over RCU consumption
- Stream results to disk (CSV + NDJSON) during the scan so partial progress is preserved
- Ship as a single statically-linked binary

## 4. Non-goals (v1)

- Remediation of any kind (no item deletion, no attribute editing, no bulk rules)
- LSI type/size violation checks (DynamoDB rejects these at write time; they cannot exist in practice)
- Multi-table batch scans within a single invocation
- CI / scheduled / fully-headless operation (TUI is always the primary surface)
- Item collection size limit checks (10GB per LSI collection)
- Value-constraint validation beyond index key shape
- Cross-account / assumed-role authentication
- Scan resumability after crash or cancel
- Auto-scaling-aware rate limits (capacity is snapshotted at scan start)

## 5. User Profile

Experienced engineers and developers working in a terminal. DynamoDB fluency is assumed — no tooltips, tutorials or hand-holding. Expected usage contexts:

- **Ad-hoc investigation.** Running against a dev or prod table from a laptop to answer "what would break if I added this GSI?"
- **Ops / migration window.** Long-running scan during a planned change window, possibly left unattended in a tmux session.
- **Platform-team distribution.** An internal tooling team ships the binary and TOML config files to developers across the organisation as a standard pre-flight check.

## 6. Functional Requirements

### 6.1 Violation detection

#### 6.1.1 GSI checks (per index, both existing and hypothetical)

For each enabled GSI:

- **Type mismatch** (always on): item has the key attribute, but the DynamoDB type code (`S`/`N`/`B`) does not match the index's declared type.
- **Size violation** (always on): partition key value exceeds 2048 bytes (UTF-8 for strings, raw bytes for binary); sort key value exceeds 1024 bytes.
- **Missing key attribute** (per-index toggle, off by default): item has no value for the index's partition or sort key. Off by default to respect sparse GSIs; can be enabled per-index when the user knows the index should be dense.

Hypothetical GSI declaration is authored in the TUI setup screen (form with name, PK attribute + type, optional SK attribute + type) and persisted to TOML. It appears alongside existing indexes in the toggle list, visually tagged. Sparse-intent is not a separate flag — it is expressed by leaving `check_missing` off.

#### 6.1.2 LSI checks

- **Missing key attribute** (per-index toggle, same `check_missing` semantics as GSI): items missing the LSI sort key attribute. Off by default to respect sparse LSIs. No type or size checks exist for LSIs (DynamoDB validates these at write time; violations cannot exist in normal operation).

#### 6.1.3 TTL checks (global toggle per scan)

- Missing TTL attribute
- Wrong type (not `N`)
- Millisecond magnitude (value > ~10^11, classic `Date.now()` bug)
- Malformed value (zero, negative, non-integer)
- Ignored by DynamoDB due to > 5 years in the past (toggleable sub-check)

"Expired but still present" (eventual-consistency lag) is out of scope.

#### 6.1.4 Output hierarchy

Violations are grouped in the TUI as: **Target (Index name | LSI name | TTL) → Violation category → Individual items**. Each GSI and LSI is a separate top-level bucket; TTL is a single global bucket parallel to them.

### 6.2 Scanning and performance

#### 6.2.1 Single-pass multi-check

One scan invocation targets a single table and performs all enabled checks (every GSI, LSI, TTL toggled on) in a single pass. Each item is read once and evaluated against every enabled rule.

#### 6.2.2 Parallel scan

- Uses DynamoDB's `Scan` with `TotalSegments` + `Segment`.
- Default segment count: number of CPUs. User-configurable in CLI, TOML, and TUI setup.
- Eventually-consistent reads (0.5 RCU per 4KB).

#### 6.2.3 Rate limiting

- **Default**: unlimited.
- **Provisioned tables**: user may set a percentage of provisioned RCU (e.g. 60%). Tool reads provisioned capacity via `DescribeTable` at scan start, uses `ReturnConsumedCapacity=TOTAL` on every `Scan` call, and paces a shared token bucket across all segment workers to stay under the ceiling.
- **On-demand tables**: unlimited regardless of any percentage setting (no fixed ceiling to meter against).
- **Capacity snapshot**: provisioned RCU is captured once at scan start. Auto-scaling adjustments during the scan are not tracked.
- **Throttling backoff**: relies on the AWS SDK for Rust's default retry middleware for any residual throttle responses.

#### 6.2.4 Progress estimation

Progress percentages are best-effort, derived from `items-scanned / DescribeTable.ItemCount`. `ItemCount` is approximate (updated every ~6 hours by DynamoDB), so the bar may overshoot 100% or finish early. TUI labels should reflect this (e.g. `~45%`).

### 6.3 TUI flow

#### 6.3.1 Screen flow

Linear: **Launch/profile picker → Scan setup → In-flight scan → Completed**. No back navigation from the completed screen — the user exits and restarts for a fresh scan.

#### 6.3.2 Launch / profile picker

- If invoked with `--config <path>`, `--profile <name>`, or other overrides, skip this screen.
- Otherwise: list AWS profiles discovered from `~/.aws/config`. User picks one. Region is pre-filled from the profile and can be overridden on the setup screen.

#### 6.3.3 Setup screen

- **Table selection**: fuzzy-search list of tables from `ListTables`. User can type to filter or select directly.
- **Discovered indexes**: on table selection, `DescribeTable` populates GSIs, LSIs, and TTL attribute (if any). Each appears as a toggle row with sub-toggles for the applicable check types.
- **Hypothetical GSI**: `+ Add hypothetical GSI` button opens a form (name, PK attribute + type, optional SK attribute + type). Added indexes appear alongside existing ones, visually tagged as hypothetical.
- **Scan settings**: segment count (defaults to CPU count), rate-limit percentage (default unlimited).
- **Export settings**: toggles for CSV and NDJSON (both on by default), output paths (defaults to `./violations-{table}-{timestamp}.{ext}`).
- **Region override** field.
- **Cost estimate** button calls `DescribeTable`, computes estimated RCU consumption for the scan and approximate duration given the configured segment count and rate limit.
- **Start scan** button begins the scan and transitions to the in-flight screen.
- **Save config** button writes the current setup to a TOML file.

#### 6.3.4 In-flight scan screen

Single screen with two regions:

- **Fixed header (stats)**: items scanned, items/sec, total violations found (aggregate), RCU consumed and rate, time elapsed, estimated time remaining.
- **Swappable body** (toggle with Tab):
  - **Detailed progress**: per-segment progress bars, per-category violation counts.
  - **Live violations feed**: rolling window of the last 1000 violations as they stream in.

Cancelling mid-scan (`Ctrl+C`) prompts for confirmation, then terminates cleanly. Whatever has been written to the export file(s) remains on disk.

#### 6.3.5 Completed screen

- Final counts per category.
- Browsable rolling window of the last 1000 violations (same widget as the in-flight feed, now static).
- Paths to the export files.
- Drill-in to a violation: detail view that re-fetches the current item via `GetItem` using the persisted PK/SK. If the item is gone or has changed, the detail view indicates this. Clipboard copy (`y`) for item PK, violating attribute, or full item JSON.

#### 6.3.6 Keybindings

- `q` / `Esc` — quit or back
- `Tab` / `Shift+Tab` — swap in-flight body view
- `↑`/`↓` or `j`/`k` — list navigation
- `Enter` — drill into selection
- `y` — copy (yank)
- `?` — help overlay
- `Ctrl+C` — cancel running scan (confirmation required)

No mouse support.

#### 6.3.7 Error UX

Terminal errors (auth failure, table not found, permission denied, SSO expired) display a modal with:

- The error text and SDK code.
- A suggested remediation (e.g. "Run `aws sso login --profile X` and retry").

Transient errors handled by SDK retry middleware do not escalate to the UI unless the retry budget is exhausted.

### 6.4 Authentication and region

- Default AWS credential provider chain (env, shared config, SSO, IMDS, container).
- SSO supported implicitly — user runs `aws sso login --profile X` before launching the tool; SDK picks up cached tokens.
- Profile override available via `--profile`, TOML `profile = "..."`, or the TUI profile picker.
- Region defaults from profile / env; overridable via `--region`, TOML `region = "..."`, or TUI setup.
- IAM permissions required (v1, detect-only): `dynamodb:Scan`, `dynamodb:DescribeTable`, `dynamodb:GetItem` (for violation detail view), `dynamodb:ListTables` (for setup picker).

### 6.5 Configuration

- **TOML** is the config file format. Default path `./scan.toml`; override via `--config <path>`.
- Config captures everything in the setup screen: profile, region, table, enabled checks per index, hypothetical GSI specs, TTL toggles, export settings, scan settings.
- TUI can both **load** from a config and **save** current setup to one.
- **Precedence**: CLI args override TOML values; TOML values override built-in defaults.

### 6.6 Export

- **CSV**: one row per violation. Columns include table, index/target, violation category, `pk` and `sk` as separate columns, optional `pk_type` / `sk_type` for type preservation, violating attribute name, actual value, actual type, expected type, size bytes (where relevant), detected-at timestamp. Binary values base64-encoded.
- **NDJSON**: one object per item, carrying a `violations` array for items with multiple violations. PK/SK preserved in native DynamoDB JSON shape (`{"S": "..."}`, `{"N": "42"}`, `{"B": "..."}`). Timestamps and metadata duplicated per-item for self-contained records.
- Row-count across formats will not match for items with multiple violations; documented behaviour.
- Both formats written simultaneously in one scan (each toggleable off).
- Files are streamed during the scan, not buffered — a partial file on crash/cancel contains everything scanned up to that point.
- Default filename: `violations-{table}-{timestamp}.{csv|ndjson}` in CWD. Overridable per-scan and per-format.

## 7. Non-functional Requirements

- **Language / runtime**: Rust, stable toolchain.
- **Primary dependencies** (proposed): `aws-config` + `aws-sdk-dynamodb` for AWS; `ratatui` + `crossterm` for TUI; `tokio` for async; `serde` + `toml` for config; `csv` and `serde_json` for export.
- **Distribution**: single statically-linked binary. Target platforms: macOS (arm64/x86_64), Linux (x86_64/arm64), Windows (x86_64).
- **Memory bounds**: violations stream to disk; in-memory retention is a fixed rolling window of the last 1000 violations (not configurable). Counts per category are O(categories). Total memory is bounded regardless of violation count.
- **Concurrency**: parallel scan via async tasks on a Tokio runtime; one logical worker per segment. Shared state (counters, rate limiter, rolling window) uses lock-free or mutex-protected primitives as appropriate.

## 8. Architecture — Module Breakdown

Seven deep modules plus a thin application shell. Each module has a narrow interface and a stable contract, testable in isolation.

### 8.1 Violation Rule Engine

**Interface (sketch)**:
```rust
fn check_item(item: &Item, rules: &RuleSet) -> Vec<Violation>;
```

**Encapsulates**: all rule logic — type comparison, UTF-8 byte counting, configurable missing-key detection, TTL magnitude classification, zero/negative/malformed TTL detection. Pure, synchronous, no I/O.

**Testability**: table-driven unit tests over synthetic items and rule sets. This is the heaviest-tested module.

### 8.2 AWS Client Facade

**Interface (sketch)**:
```rust
#[async_trait]
trait DynamoClient {
    async fn list_tables(&self) -> Result<Vec<String>>;
    async fn describe_table(&self, name: &str) -> Result<TableDescription>;
    async fn scan_segment(&self, req: ScanRequest) -> Result<ScanResponse>;
    async fn get_item(&self, req: GetItemRequest) -> Result<Option<Item>>;
}
```

**Encapsulates**: SDK construction, credential chain, region wiring, SDK error mapping. Downstream code holds `Arc<dyn DynamoClient>`.

**Testability**: mock implementations drive every test of modules that consume it.

### 8.3 Scan Driver

**Interface (sketch)**:
```rust
fn run_scan(config: ScanConfig, client: Arc<dyn DynamoClient>) -> impl Stream<Item = ScannedItem>;
```

**Encapsulates**: parallel segment fan-out, per-segment pagination via `LastEvaluatedKey`, shared-token-bucket rate limiting against the RCU budget, `ConsumedCapacity` aggregation, throttling backoff, stream back-pressure, graceful shutdown.

**Testability**: mock AWS client simulates paginated responses, throttling, partial failures. Assertions on segment counts, rate-limit adherence, clean termination.

### 8.4 Export Writer

**Interface (sketch)**:
```rust
trait ExportWriter {
    fn write(&mut self, group: &ItemViolations) -> Result<()>;
    fn close(self: Box<Self>) -> Result<()>;
}
```

Implementations: `CsvWriter`, `NdjsonWriter`, and a `FanOutWriter` that wraps N writers.

**Encapsulates**: format-specific serialisation, PK/SK column splitting and type preservation, buffered writes and flushing, partial-file semantics on drop.

**Testability**: sink to an in-memory buffer, assert on exact byte output for representative inputs.

### 8.5 State / Progress Aggregator

**Interface (sketch)**:
```rust
fn record_item(&self, segment: u32);
fn record_violation(&self, v: &Violation);
fn record_consumed(&self, segment: u32, rcu: f64);
fn snapshot(&self) -> StateSnapshot;
```

**Encapsulates**: thread-safe atomic counters, rolling window of last 1000 violations, items/sec over a sliding time window, RCU rate, ETA estimation.

**Testability**: drive events, assert snapshot contents including time-derived fields (with injectable clock).

### 8.6 TUI Renderer

**Interface (sketch)**:
```rust
fn render(state: &StateSnapshot, frame: &mut Frame);
fn handle_event(&mut self, event: Event) -> Option<Command>;
```

Commands are a small enum: `StartScan`, `CancelScan`, `LoadConfig`, `SaveConfig`, `CopyToClipboard`, `DrillInto(ViolationId)`, `SwapView`, `Quit`, and similar.

**Encapsulates**: ratatui layout, widgets, screen transitions, keybinding dispatch, modal management.

**Testability**: event-to-command unit tests; ratatui test backend for rendering snapshots.

### 8.7 Config Loader

**Interface (sketch)**:
```rust
fn load(path: Option<&Path>, cli: &CliArgs) -> Result<ScanConfig>;
fn save(cfg: &ScanConfig, path: &Path) -> Result<()>;
```

**Encapsulates**: TOML parsing, CLI-over-TOML merge precedence, default resolution, schema validation.

**Testability**: fixture-based — load sample TOML files and assert resolved configs; round-trip save/load.

### 8.8 Application shell

Deliberately thin. `main` and the top-level event loop wire the modules: Config Loader → AWS Client Facade → TUI event loop → (on Start) Scan Driver → Rule Engine → (Export Writer, State Aggregator). Contains no business logic of its own.

## 9. Deferred to v2+

- Remediation flows (per-item TUI edits and bulk rule-based fixes).
- Scan resumability with checkpointing.
- Auto-scaling-aware rate limiting.
- Cross-account / assumed-role authentication.
- Headless / CI mode with structured stdout and non-zero exit codes on violations.
- Multi-table batch scans from a single config.
- On-demand table "soft" rate limiting (percentage of observed throughput).
- Detection-time state snapshot stored alongside violations so the detail view can contrast current-vs-detected.

## 10. Appendix: TOML config schema

Illustrative example showing every supported field. Precise field names, optionality and validation rules will be finalised alongside the first Config Loader implementation.

```toml
# Required: table to scan
table = "users"

# Optional: AWS targeting. Defaults come from the active credential chain.
region = "eu-west-1"
profile = "prod-readonly"

# Scan behaviour
[scan]
segments = 16                      # default: CPU count
rate_limit_percent = 60            # optional; unlimited if omitted or if the table is on-demand

# Export targets
[export]
csv = true
csv_path = "./violations.csv"      # optional; default ./violations-{table}-{timestamp}.csv
ndjson = true
ndjson_path = "./violations.ndjson"

# TTL audit (global toggle + per-check sub-toggles)
[ttl]
enabled = true
check_missing = true
check_wrong_type = true
check_ms_magnitude = true
check_malformed = true
check_past_5_years = false

# Existing GSI. Type and size checks are always on; only the missing-key toggle is configurable.
[[gsi]]
name = "GSI1"
check_missing = false              # true only for non-sparse indexes

# Hypothetical (pre-flight) GSI
[[gsi]]
name = "GSI_proposed_userId_createdAt"
hypothetical = true
pk = { name = "userId", type = "S" }
sk = { name = "createdAt", type = "N" }   # optional; omit for partition-key-only GSIs
check_missing = true                      # sparse-intent is expressed by setting this to false

# LSI missing-key check (sparse detection)
[[lsi]]
name = "LSI1"
check_missing = true
```

Design notes on the schema:

- Existing vs. hypothetical GSIs share a single `[[gsi]]` array discriminated by `hypothetical = true`. Keeps the config flat and makes it trivial to promote a hypothetical entry to a real one (delete `hypothetical`, `pk`, `sk` once the GSI is created).
- Key attribute types use DynamoDB native type codes (`"S"`, `"N"`, `"B"`) rather than long-form names, matching the SDK.
- `check_missing` is unified across GSI and LSI — semantically the same check (item lacks the index's key attribute). Sparse indexes are expressed by `check_missing = false`; there is no separate `sparse` flag.
- Type and size checks are always on for GSIs (violations of these can only come from historical data predating the index, and there is no operator reason to suppress them). Only `check_missing` is exposed as a per-index toggle.
- Precedence of CLI flag overrides over file values is handled in the Config Loader.
