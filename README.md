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

With no arguments the tool opens the AWS profile picker, then the scan setup
screen. Any of `--config`, `--table`, `--profile`, or `--region` skips the
picker and pre-fills setup.

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

Required IAM permissions (detect-only): `dynamodb:Scan`, `dynamodb:DescribeTable`,
`dynamodb:GetItem`, `dynamodb:ListTables`.

### Keybindings

- `q` / `Esc` — quit or back
- `Tab` / `Shift+Tab` — swap the in-flight body view
- `↑`/`↓` or `j`/`k` — navigate lists
- `Enter` — drill into selection
- `y` — copy (yank)
- `?` — help overlay
- `Ctrl+C` — cancel a running scan (confirmation required)

## Configuration

Scan setup is captured in a TOML file (default `./scan.toml`, override with
`--config`). The setup screen can both load from and save to one. See `PRD.md`
§10 for the full schema; a minimal example:

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
