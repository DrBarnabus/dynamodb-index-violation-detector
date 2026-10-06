# DynamoDB Index Violation Detector

A terminal tool for scanning a DynamoDB table to find items that violate the key
schema of a GSI (existing or hypothetical), the key schema of an LSI, or the
expected shape of a TTL attribute. Violations are reviewed in a TUI and streamed
to CSV and NDJSON export files for downstream remediation.

It fills the gap left by the archived `awslabs/dynamodb-online-index-violation-detector`,
targeting engineers who need to audit a table before adding a GSI or to
investigate an existing index. When a GSI is added to a pre-existing table,
DynamoDB backfills it by scanning existing items; items whose proposed-key
attributes are missing, of the wrong type, or over the index key size limits are
silently not indexed. This tool reports those items up front.

## Install

Prebuilt binaries for Linux (x86_64 and arm64, statically linked against musl),
macOS (Apple Silicon) and Windows (x86_64) are attached to each
[GitHub Release](https://github.com/DrBarnabus/dynamodb-index-violation-detector/releases),
alongside a `SHA256SUMS` file:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar xzf dynamodb-violation-detector-<version>-<target>.tar.gz
```

## Build

Requires [rustup](https://rustup.rs), which installs the Rust version pinned in
`rust-toolchain.toml`.

```sh
cargo build --release
```

The binary is written to `target/release/dynamodb-violation-detector`. The
release profile enables LTO and symbol stripping, producing a self-contained
binary that links only platform system libraries.

On macOS a fully static binary is not possible — the system C library and
frameworks are always dynamically linked; the produced binary depends only on
those. For a static Linux binary, build against musl:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Run

```sh
dynamodb-violation-detector [OPTIONS]
```

With no config file, no `AWS_PROFILE` and no flags, the tool opens on a profile
picker listing the profiles in `~/.aws/config` and `~/.aws/credentials` (or the
files named by `AWS_CONFIG_FILE` / `AWS_SHARED_CREDENTIALS_FILE`). The chosen
profile's region pre-fills the setup screen. Otherwise it opens straight on the
scan setup screen, pre-filled from the config file and flags.

On the setup screen, the table field filters the account's tables as you type;
choosing one describes it and lists its indexes and TTL attribute. A table name
from `--table` or the config file is described on launch. AWS errors (expired
credentials, missing permissions, an unknown table) are shown in a modal with a
suggested fix.

| Flag | Description |
| --- | --- |
| `--config <PATH>` | TOML config file (default: `./scan.toml` if present) |
| `--table <TABLE>` | Table to scan |
| `--profile <PROFILE>` | AWS profile |
| `--region <REGION>` | AWS region |
| `--segments <N>` | Parallel scan segment count (default: CPU count) |
| `--rate-limit-percent <1..=100>` | Percentage of provisioned RCU to consume (unlimited if unset) |
| `-V`, `--version` | Print the version and the git commit it was built from |

CLI flags override TOML values, which override built-in defaults.

### Credentials

Uses the default AWS credential provider chain (environment, shared config, SSO,
IMDS, container). For SSO, run `aws sso login --profile <name>` before launching.
Region defaults from the profile or environment and is overridable per scan.
Editing the region on the setup screen reconnects once you leave the field,
refreshing the table list and re-describing the chosen table in that region.

Required IAM permissions (detect-only): `dynamodb:Scan`, `dynamodb:DescribeTable`,
`dynamodb:ListTables` for the table picker and `dynamodb:GetItem` for the
violation detail view. Without `ListTables`, type the table name in full.

### Keybindings

`?` toggles the help overlay on every screen.

Profile picker:

- Type to filter, `↑`/`↓` to move, `Enter` to choose, `Esc` to quit

Setup screen:

- `Tab` / `↓` and `Shift+Tab` / `↑` — move between fields
- On the table field, type to filter; `↑`/`↓` move through matching tables and
  `Enter` chooses the highlighted one (or the typed name when nothing matches)
- `Space` — toggle a checkbox
- `Enter` — next field, open the add-form on *+ Add hypothetical GSI*, estimate
  the scan on *Estimate cost*, or start the scan on *Start scan*
- `Delete` / `Backspace` on a hypothetical GSI — remove it
- `Ctrl+S` — save the form to the config file
- `Esc` — quit

The *Add hypothetical GSI* form takes an index name, a partition key attribute
and type, and an optional sort key attribute and type. `Tab` / `↑` / `↓` move
between fields, `Space` or `←`/`→` change a key type, `Enter` on *Add index*
adds it, and `Esc` cancels. Added indexes are tagged `[hypothetical]` and saved
with `Ctrl+S`.

*Estimate cost* re-describes the table and shows the RCU a full scan should
consume (half an RCU per 4 KB, eventually consistent) and its duration at the
form's rate limit. On-demand tables have no capacity ceiling, so their duration
assumes about 1,280 RCU/s per segment. The size comes from `DescribeTable`,
which DynamoDB refreshes roughly every 6 hours. Editing the table, region,
segments or rate limit clears the estimate.

In-flight screen:

- `Tab` — swap the body between detailed progress (per-category counts and
  per-segment bars) and the live feed of the last 1000 violations, each shown
  with its item's key
- `Ctrl+C`, `q` or `Esc` — cancel the scan; confirm with `y`, dismiss with `n` / `Esc`

Completed screen:

- `↑`/`↓` or `j`/`k` — move through the feed of recent violations, or scroll
  the item in the detail view
- `Enter` — open the detail view: the item is re-fetched by key, showing
  whether it has since been deleted or changed and whether the violation still
  holds
- `y` then `p`, `a` or `j` — copy the item's primary key (as DynamoDB JSON), the
  violating attribute's name, or, in the detail view, the full item JSON
- `q` / `Esc` — leave the detail view, or quit

Copying uses the OSC 52 terminal escape, so it works over SSH in terminals that
support it (iTerm2, WezTerm, kitty, Alacritty, Windows Terminal; tmux with
`set-clipboard on`). macOS Terminal.app ignores it.

## Configuration

Scan setup is captured in a TOML file (default `./scan.toml`, override with
`--config`). It pre-fills the setup screen at launch, and `Ctrl+S` on that
screen saves the form back to it. A minimal example:

```toml
table = "users"
region = "eu-west-1"

[scan]
segments = 16
rate_limit_percent = 60        # optional; ignored for on-demand tables

[export]
csv = true
ndjson = true

[ttl]
enabled = true

[[gsi]]
name = "GSI1"
check_missing = false          # true only for non-sparse indexes

[[gsi]]
name = "ByEmail"
hypothetical = true            # audit a GSI before creating it
pk = { name = "email", type = "S" }
```

## Export

Both formats are streamed to disk during the scan, so a partial file on
cancel or crash contains everything scanned up to that point. Default filenames
are `violations-{table}-{timestamp}.{csv,ndjson}` in the working directory.

- **CSV** — one row per violation; PK/SK as separate columns, binary values
  base64-encoded.
- **NDJSON** — one object per item, with a `violations` array; PK/SK preserved in
  native DynamoDB JSON shape.

## Testing against a local table

Point the tool at [DynamoDB Local](https://hub.docker.com/r/amazon/dynamodb-local)
via the standard endpoint override:

```sh
docker run -d -p 8000:8000 amazon/dynamodb-local
AWS_ENDPOINT_URL=http://localhost:8000 \
AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x \
  dynamodb-violation-detector --table users --region eu-west-1
```

## Tests

```sh
cargo test
```

Unit tests run against a mock client. The integration tests in
`tests/dynamodb_local.rs` start DynamoDB Local in Docker for each test via
`testcontainers`, so they need a running Docker daemon. To run only the unit
tests, use `cargo test --lib --bins`.

## Benchmarks

```sh
cargo bench
```

[Criterion](https://github.com/bheisler/criterion.rs) benchmarks cover the rule
engine (`benches/rules.rs`) and the export writers (`benches/export.rs`), each
over batches of 1,000 items. Criterion flags pass through after `--`: `--quick`
for a fast run, or `--save-baseline <name>` and `--baseline <name>` to compare
against an earlier run. Reports are written to `target/criterion/`.

## Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0
git push origin v0.2.0
```

Pushing the tag runs CI, checks the tag against `Cargo.toml`, builds each target
and drafts a release for you to review and publish; tags with a `-` suffix such
as `v0.2.0-rc.1` become pre-releases. Running the workflow manually from the
Actions tab only builds the archives.
