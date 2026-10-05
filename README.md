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

## Build

Requires a stable Rust toolchain.

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

CLI flags override TOML values, which override built-in defaults.

### Credentials

Uses the default AWS credential provider chain (environment, shared config, SSO,
IMDS, container). For SSO, run `aws sso login --profile <name>` before launching.
Region defaults from the profile or environment and is overridable per scan.
Editing the region on the setup screen reconnects once you leave the field,
refreshing the table list and re-describing the chosen table in that region.

Required IAM permissions (detect-only): `dynamodb:Scan`, `dynamodb:DescribeTable`,
and `dynamodb:ListTables` for the table picker. Without `ListTables`, type the
table name in full.

### Keybindings

`?` toggles the help overlay on every screen.

Profile picker:

- Type to filter, `↑`/`↓` to move, `Enter` to choose, `Esc` to quit

Setup screen:

- `Tab` / `↓` and `Shift+Tab` / `↑` — move between fields
- On the table field, type to filter; `↑`/`↓` move through matching tables and
  `Enter` chooses the highlighted one (or the typed name when nothing matches)
- `Space` — toggle a checkbox
- `Enter` — next field, or start the scan on *Start scan*
- `Ctrl+S` — save the form to the config file
- `Esc` — quit

In-flight screen:

- `Ctrl+C`, `q` or `Esc` — cancel the scan; confirm with `y`, dismiss with `n` / `Esc`

Completed screen:

- `↑`/`↓` or `j`/`k` — move through the violation summary
- `q` / `Esc` — quit

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
