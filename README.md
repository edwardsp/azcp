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
3. Ambient Azure CLI login (`az login`) — Bearer token via `az account get-access-token`
4. Anonymous (public containers only)

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

Two independent knobs control parallelism within a single process:

- `--concurrency N` (default 64): max in-flight HTTP block requests across all files.
- `--parallel-files N` (default 16, env `AZCP_PARALLEL_FILES`): max files actively transferring at once. Files share the `--concurrency` budget for their chunks.

Raising `--parallel-files` lets multiple files dispatch their chunks concurrently rather than sequentially, which significantly improves download throughput on small file counts. For 16 large files at 100 GbE, `--parallel-files 16 --concurrency 128 --block-size 33554432` is a good starting point.

A single `azcp` process saturates around 28-29 Gbps on download due to distributed CPU cost across hyper/TLS/tokio (profile-driven; no single hotspot > 15%). To go higher, run multiple independent tokio runtimes in one process with `--workers N`:

```bash
# Single invocation, 4 in-process workers → ~52 Gbps on 16 × 2 GiB files
azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
  --recursive --workers 4 --concurrency 32 --parallel-files 4 \
  --block-size 16777216
```

Each worker gets its own tokio runtime, reqwest connection pool, and shard of the
file list (workers auto-shard by `INDEX/COUNT`). This recaptures ~90% of the
multi-process throughput (57 Gbps) without external orchestration.

If you need to shard across multiple *nodes* (not just runtimes), use
`--shard INDEX/COUNT` instead:

```bash
# 4 external processes, one per node, ~57 Gbps aggregate
for i in 0 1 2 3; do
  azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst$i/ \
    --recursive --shard $i/4 --concurrency 32 --parallel-files 4 \
    --block-size 16777216 &
done
wait
```

Both flags partition files deterministically (sorted by name, every Nth entry) so
shards never overlap. `--workers` and `--shard` work on both upload and download.

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
