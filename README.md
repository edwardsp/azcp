# azcp

A Rust port of [`azcopy`](https://github.com/Azure/azure-storage-azcopy) focused on Azure Blob Storage. Fast, parallel, and with a small, predictable CLI.

## Features

- **copy** — local ↔ blob, recursive, resumable per-file, configurable block size and concurrency
- **sync** — one-way sync with four diff strategies: `size`, `size-and-mtime`, `md5`, `always`
- **ls** — list containers / blobs, with `<DIR>` rollups in non-recursive mode
- **rm** — parallel blob deletion with glob filters and progress
- **mk** — create containers
- Parallel block uploads / downloads with a single global concurrency budget
- `--include-pattern` / `--exclude-pattern` glob filters on all bulk commands
- Live progress bar tracking bytes, throughput, ETA, and file counts
- Authentication via Shared Key, SAS, Bearer token, or Azure CLI ambient credentials

## Installation

### Pre-built binaries

Download the archive for your platform from the [latest release](../../releases/latest):

| Platform | Target |
|---|---|
| Linux x86_64 | `azcp-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `azcp-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `azcp-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `azcp-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `azcp-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `azcp-aarch64-pc-windows-msvc.zip` |

Each archive contains the `azcp` binary plus a SHA256 sidecar for verification.

### From source

```bash
cargo build --release
# binary at ./target/release/azcp
```

Requires Rust 1.75+.

## Authentication

`azcp` resolves credentials in this order:

1. `AZURE_STORAGE_ACCOUNT` + `AZURE_STORAGE_KEY` — Shared Key
2. `AZURE_STORAGE_SAS_TOKEN` — SAS
3. **AKS workload identity** — when `AZURE_FEDERATED_TOKEN_FILE` + `AZURE_CLIENT_ID` + `AZURE_TENANT_ID` are set (automatic in workload-identity-enabled pods)
4. **Azure VM managed identity** via IMDS (`169.254.169.254`). If multiple user-assigned identities are attached, set `AZURE_CLIENT_ID` to select one.
5. Ambient Azure CLI login (`az login`) — account key then Bearer token
6. Anonymous (public containers only)

Run `azcp env` to see which credential source is active.

## Usage

All commands accept standard blob URLs of the form:

```
https://<account>.blob.core.windows.net/<container>/<path>
```

### copy

```bash
# Upload a directory
azcp copy ./local/dir https://acct.blob.core.windows.net/ctr/path --recursive

# Download
azcp copy https://acct.blob.core.windows.net/ctr/path ./local/dir --recursive

# Tune parallelism
azcp copy ./big https://acct.blob.core.windows.net/ctr/ \
  --recursive --concurrency 64 --block-size 8388608 --progress

# Filter with globs
azcp copy ./src https://acct.blob.core.windows.net/ctr/backup/ \
  --recursive --include-pattern '*.rs' --exclude-pattern 'target/*'
```

Flags: `--recursive`, `--no-overwrite`, `--block-size`, `--concurrency`, `--parallel-files`, `--workers`, `--shard`, `--max-retries`, `--dry-run`, `--check-md5`, `--include-pattern`, `--exclude-pattern`, `--progress`.

### Throughput tuning

Two independent knobs control parallelism:

- `--workers N` (default 1): spawn N independent tokio runtimes in one process, each with its own `reqwest` connection pool and its own shard of the workload. **Required to scale past ~28 Gbps** — a single tokio runtime + reqwest client tops out there regardless of how many concurrent requests you submit (see "why workers are necessary" below).
- `--concurrency N` (default 64) and `--parallel-files N` (default 16, env `AZCP_PARALLEL_FILES`): per-runtime limits on in-flight HTTP block requests and actively-transferring files. With `--workers > 1`, these are per-worker.

For 16+ large files at 100+ GbE, `--workers 4 --concurrency 32 --parallel-files 4 --block-size 16777216` reaches **~64 Gbps** sustained on download; `--workers 8` peaks at **~69 Gbps**.

```bash
# High-throughput download (320 GiB / 160 files measured at 64 Gbps sustained)
azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
  --recursive --workers 4 --concurrency 32 --parallel-files 4 \
  --block-size 16777216
```

Each worker auto-shards files using **size-balanced LPT** (Longest-Processing-Time bin packing): files are sorted by size descending and greedily assigned to the least-loaded worker. For a 385 GiB / 175-file DeepSeek-R1 checkpoint with 3 × 8-9 GiB + 172 × ~2.3 GiB shards, this brings per-worker load spread from 31% (naive round-robin) to 1.3%, eliminating stragglers.

#### Why workers are necessary (not just a tuning knob)

A single tokio runtime + single `reqwest::Client` has a hard ceiling around **25-28 Gbps** on Azure Blob downloads, *regardless of any tuning*. Measured on a 128-core node, single-runtime with all 160 files actively transferring and 2048 in-flight requests:

| `--concurrency` / `--parallel-files` | Gbps |
|---|---|
| 128 / 16 | 26.1 |
| 512 / 64 | 26.2 |
| 1024 / 128 | 27.1 |
| 2048 / 160 (every file in flight) | 26.5 |

The bottleneck is serialized work inside hyper's connection pool and tokio's scheduler that no amount of submission parallelism can speed up. `--workers N` gives each shard its own runtime + own pool, which is the only way past it.

#### Multi-node sharding

For sharding across multiple *nodes* (not runtimes within one node), use `--shard INDEX/COUNT`:

```bash
# One process per node, deterministic file partitioning
azcp copy ... --shard $NODE_IDX/$NODE_COUNT --workers 4 ...
```

`--workers` and `--shard` compose: each node runs N workers over its own slice.

### Allocator

Releases ship with **jemalloc** as the default global allocator (Linux/macOS — Windows MSVC silently uses the system allocator). Measured +5-7% throughput vs glibc malloc on sustained multi-worker downloads.

To opt out (system malloc only):
```bash
cargo build --release --no-default-features
```

Mimalloc is also available (no measurable improvement over glibc for this workload, but kept as an option):
```bash
cargo build --release --no-default-features --features mimalloc-allocator
```

### Throttling and retries

Every operation retries `503 ServerBusy`, `429 Too Many Requests`, and transient
`5xx` responses with exponential backoff + jitter (honoring `Retry-After`). The
retry budget is controlled by `--max-retries` (default 5, env `AZCP_MAX_RETRIES`).

When `--progress` is on, the total bar suffix shows live retry counters, e.g.
`[retry 503x12 429x2]`, so you can tell at a glance that Azure is pushing
back. If uploads ultimately fail after exhausting retries, `azcp` exits with
a non-zero status and prints a `Failed: N` summary.

On very high-bandwidth endpoints you can raise `--concurrency` aggressively,
but if you see throttle counts climb, Azure is telling you you've hit the
account ingress limit (typically 60 Gbps for standard storage). Lower
concurrency or request a quota increase.

### sync

One-way synchronization with a choice of diff strategies:

```bash
# Default: size + mtime (fast, accurate for most workflows)
azcp sync ./local https://acct.blob.core.windows.net/ctr/prefix

# Content-hash mode (catches same-size changes; reads local files)
azcp sync ./local https://acct.blob.core.windows.net/ctr/prefix \
  --compare-method md5

# Size-only (cheapest)
azcp sync ./local https://acct.blob.core.windows.net/ctr/prefix \
  --compare-method size

# Remove remote files that no longer exist locally
azcp sync ./local https://acct.blob.core.windows.net/ctr/prefix \
  --delete-destination
```

| `--compare-method` | Detects | Reads files? |
|---|---|---|
| `size` | Size change | No |
| `size-and-mtime` (default) | Size change or newer local mtime | No |
| `md5` | Content change at same size | Yes (local) |
| `always` | Everything always re-transferred | No |

**MD5 caveat:** Azure populates `Content-MD5` automatically only for single-shot (small-file) uploads. Large block-list uploads need `--check-md5` on the original `copy` for `--compare-method md5` to have anything to compare against. If any remote blob lacks `Content-MD5`, `sync --compare-method md5` fails fast with an actionable error listing the offending blobs.

Sync works in both directions: swap source and destination to pull blobs down to a local tree.

### ls

```bash
# Containers in an account
azcp ls https://acct.blob.core.windows.net/

# Non-recursive (shows <DIR> rollups)
azcp ls https://acct.blob.core.windows.net/ctr/path/

# Recursive flat listing
azcp ls https://acct.blob.core.windows.net/ctr/path/ --recursive
```

### rm

```bash
# Single blob
azcp rm https://acct.blob.core.windows.net/ctr/file.bin

# Recursive with progress and filters
azcp rm https://acct.blob.core.windows.net/ctr/prefix \
  --recursive --progress --include-pattern '*.log'
```

### mk

```bash
azcp mk https://acct.blob.core.windows.net/new-container
```

## Configuration

Environment variables recognized by `azcp`:

| Variable | Purpose |
|---|---|
| `AZURE_STORAGE_ACCOUNT` | Account name (Shared Key auth) |
| `AZURE_STORAGE_KEY` | Account key (Shared Key auth) |
| `AZURE_STORAGE_SAS_TOKEN` | SAS token |
| `AZCOPY_CONCURRENCY_VALUE` | Default concurrency if `--concurrency` unset |
| `AZCOPY_LOG_LOCATION` | Log output directory |

Run `azcp env` to see current values.

## Testing

### Unit tests

```bash
cargo test
```

### Integration tests (require live storage)

The sync integration suite in `tests/sync_integration.rs` exercises the binary end-to-end against a real container. Point it at an account you own:

```bash
AZCP_TEST_ACCOUNT=myaccount \
AZCP_TEST_CONTAINER=test \
  cargo test --release --test sync_integration -- --nocapture
```

Tests skip cleanly when those env vars are unset. Each test uses a unique `azcp-it/<name>-<nanos>` prefix and cleans up on drop, so runs are isolated and safe in parallel.

Coverage includes: upload+rerun skip behavior, all four `--compare-method` strategies, `--delete-destination`, blob→local sync, and `--include-pattern` / `--exclude-pattern` filtering.

## Continuous Integration

`.github/workflows/build.yml` builds all six platform targets on every push/PR using native runners (no cross-compilation). Tag a release to publish binaries:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release job assembles all artifacts (tarballs/zips + SHA256 checksums) into a GitHub Release with auto-generated notes.

## Project layout

```
src/
  auth/         SharedKey, SAS, Bearer, Azure CLI credential sources
  cli/          Command definitions and argument parsing
  engine/       Transfer engine: parallel scheduler, progress, glob filtering
  storage/
    blob/       Blob REST client (list, put block, get range, delete, ...)
    local.rs    Local filesystem walk
  error.rs      Error type
  config.rs     Env-var configuration
tests/
  sync_integration.rs   End-to-end sync tests against a live account
.github/
  workflows/build.yml   Multi-platform build + release
```

## License

See `LICENSE`.
