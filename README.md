# azcp

A fast, parallel CLI and MPI cluster tool for moving data between Azure Blob
Storage and your compute. Familiar `azcopy`-style syntax, designed for the
three throughput ceilings you hit on real workloads:

1. **One process** — saturating a single tokio runtime + connection pool
   tops out around 25-28 Gbps. `azcp copy --workers N` spawns N
   independent runtimes to scale past it.
2. **One node** — past ~70 Gbps the host's overlay network, NUMA placement,
   and per-CPU softirq queues become the bottleneck. `--shard i/N`,
   `hostNetwork`, and `numactl` peel those layers back to 200 Gbps. See
   [docs/performance-tuning.md](docs/performance-tuning.md).
3. **One cluster** — when many nodes need the same dataset, paying Azure
   egress per node is wasteful. `azcp-cluster` (companion MPI binary)
   downloads `1/N` of the dataset per rank and `MPI_Bcast`s the rest over
   RDMA/IB at fabric speed, for a 5-10× end-to-end win on 10+ nodes. See
   [docs/cluster.md](docs/cluster.md).

The CLI surface is intentionally close to `azcopy` so muscle-memory carries
over: `azcp copy`, `azcp sync`, `azcp ls`, `azcp rm`, `azcp mk`. The
internals are different — Rust, single static binary per platform,
deterministic LPT sharding for parallelism that composes across processes
and nodes.

## Features

- **copy** — local ↔ blob, recursive, resumable per-file, configurable block size and concurrency
- **sync** — one-way sync with four diff strategies: `size`, `size-and-mtime`, `md5`, `always`
- **ls** — list containers / blobs, with `<DIR>` rollups in non-recursive mode
- **rm** — parallel blob deletion with glob filters and progress
- **mk** — create containers
- Parallel block uploads / downloads with a single global concurrency budget
- `--include-pattern` / `--exclude-pattern` glob filters on all bulk commands
- Live progress bar tracking bytes, throughput, ETA, and file counts
- Authentication via Shared Key, SAS, Bearer token, AKS workload identity, Azure VM managed identity, or Azure CLI ambient credentials
- **`azcp-cluster`** companion MPI binary for multi-node broadcast downloads (see [docs/cluster.md](docs/cluster.md))

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

`azcp-cluster` is shipped **as a container only** — it links Open MPI and
dlopens UCX/libibverbs at runtime, so a self-contained binary that works
against arbitrary host MPI/UCX stacks is impractical. Pull from GHCR:

```
ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.0
```

Multi-arch (`linux/amd64`, `linux/arm64`). See
[docs/cluster.md](docs/cluster.md) for AKS and Slurm deployment.

### From source

```bash
cargo build --release
# binary at ./target/release/azcp
```

Requires Rust 1.75+.

## Authentication

`azcp` resolves credentials in this order:

1. SAS token embedded in the source/destination URL (`?sv=...&sig=...`)
2. `AZURE_STORAGE_ACCOUNT` + `AZURE_STORAGE_KEY` — Shared Key
3. `AZURE_STORAGE_SAS_TOKEN` — SAS
4. **AKS workload identity** — when `AZURE_FEDERATED_TOKEN_FILE` + `AZURE_CLIENT_ID` + `AZURE_TENANT_ID` are set (automatic in workload-identity-enabled pods)
5. **Azure VM managed identity** via IMDS (`169.254.169.254`). If multiple user-assigned identities are attached, set `AZURE_CLIENT_ID` to select one.
6. Ambient Azure CLI login (`az login`) — Bearer token (RBAC) preferred over scraped account key, since accounts that disable `allowSharedKeyAccess` reject the latter with 403.
7. Anonymous (public containers only)

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
  --recursive --concurrency 64 --block-size 8388608

# Filter with globs
azcp copy ./src https://acct.blob.core.windows.net/ctr/backup/ \
  --recursive --include-pattern '*.rs' --exclude-pattern 'target/*'
```

Flags: `--recursive`, `--no-overwrite`, `--block-size`, `--concurrency`, `--parallel-files`, `--workers`, `--shard`, `--shardlist`, `--max-retries`, `--max-bandwidth`, `--dry-run`, `--check-md5`, `--include-pattern`, `--exclude-pattern`, `--progress`, `--no-progress`.

> Progress display is **on by default when stderr is a TTY** and silenced
> automatically when output is redirected (logs, CI, `kubectl logs`). Pass
> `--progress` to force it on (e.g. allocated PTY in CI, or `nohup` runs you
> intend to `tail -f`) and `--no-progress` to force it off.

### Throughput tuning

Two independent knobs control parallelism:

- `--workers N` (default 1): spawn N independent tokio runtimes in one process, each with its own `reqwest` connection pool and its own shard of the workload. **Required to scale past ~28 Gbps** — a single tokio runtime + reqwest client tops out there regardless of how many concurrent requests you submit (see "why workers are necessary" below).
- `--concurrency N` (default 64) and `--parallel-files N` (default 16, env `AZCP_PARALLEL_FILES`): per-runtime limits on in-flight HTTP block requests and actively-transferring files. With `--workers > 1`, these are per-worker.

For 16+ large files at 100+ GbE, `--workers 4 --concurrency 32 --parallel-files 4 --block-size 16777216` reaches **~64 Gbps** sustained on download from inside a default Kubernetes pod; `--workers 8` peaks at **~69 Gbps**. To go past that on faster NICs (100-200+ GbE), see [docs/performance-tuning.md](docs/performance-tuning.md) — the limiting factor at that point is the host network stack and NUMA placement, not `azcp`'s tuning knobs.

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

#### Multi-process / multi-node sharding

`--shard INDEX/COUNT` partitions the workload deterministically across `COUNT`
independent invocations. Each invocation lists the full source, runs the same
**size-balanced LPT bin packing** (Longest-Processing-Time), and keeps only
the files assigned to its `INDEX`. No coordination between processes is
needed — the partition is identical on every shard because the input list,
sort order (`size DESC, name ASC`), and tie-breakers are deterministic.

```bash
# 4 cooperating processes (one per node, or all on one host)
for i in 0 1 2 3; do
  azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
    --recursive --shard $i/4 --concurrency 32 --block-size 16777216 &
done
wait
```

**`INDEX` is 0-based.** Valid values for `--shard i/N` are `i ∈ [0, N-1]`.
`--shard 4/4` is rejected with `shard index 4 must be < count 4`.

##### `--shard` vs `--workers` (pick one)

| | `--workers N` | `--shard i/N` |
|---|---|---|
| Process model | One process, N tokio runtimes | N independent processes |
| Listing cost | 1 LIST (shared) | N LIST calls (one per process) |
| Progress / stats | Unified across workers | Per-process (you aggregate manually) |
| Fault isolation | One OOM kills all workers | Independent — one process can crash |
| Cross-host scaling | No (single process) | Yes (one process per node) |
| NUMA pinning | One process can only pin to one NUMA | Each process pins independently |

The two flags **compose**: passing `--shard X/N` alongside `--workers W`
splits this process's outer slice into `W` sub-partitions, so the cluster
as a whole runs `N × W` total partitions. Each worker `i` of process `X`
owns sub-partition `(X*W + i) / (N*W)`. This is the right behavior for
distributed launchers (MPI, torchrun, Slurm) that pass `--shard
$RANK/$WORLD_SIZE` and want intra-rank parallelism on top.

For the **same dataset on every node** (model checkpoints, training data),
prefer [`azcp-cluster`](docs/cluster.md) over per-node `--shard` — it pays
Azure egress once and broadcasts at fabric speed.

##### Sharding gotchas

- **Source must be stable during the run.** If a blob appears or disappears between two processes' listings, their owner maps diverge → some files get downloaded twice or skipped. Don't shard against a source that's being mutated.
- **Identical CLI args required.** All N invocations must use the same source URL, `--include-pattern`, `--exclude-pattern`, etc. Different filters → different input sets → different LPT result → overlap or gaps. Only `--shard i/N` should differ.
- **No cross-process retry.** If process 3 of 4 crashes, you have 3/4 of the data and the failed shard isn't picked up by anyone else. Wrap each invocation in your own retry loop, or re-run that specific `--shard 3/4` manually.
- **`N × LIST` cost.** Each process re-lists the source. Cheap for ≤10K blobs; for millions of blobs, listing dominates startup. Prefer `--workers N` (single listing) for very large file counts on a single host.
- **LPT needs `file_count >> shard_count`.** With 5 files across 4 shards, one shard may get nothing or a giant outlier dominates. As a rule of thumb, don't shard finer than ~4× the number of large files.
- **Single-blob copies ignore sharding.** Sharding partitions a *file list*. A single explicit blob URL is a one-element list; `--shard` on it is a no-op for all but `INDEX=0`.
- **`--shard 0/1` is a deliberate no-op** (defensive, so wrapper scripts don't break when `N=1`).

##### Caching the source listing with `--shardlist`

For very large source sets (millions of blobs) or for repeated transfers of
the same dataset (e.g. pulling the same model checkpoint to many nodes),
the LIST API call can dominate startup — and with `--shard i/N` it pays
that cost `N` times. `--shardlist FILE` lets you generate the listing once
and reuse it across runs and shards, skipping LIST entirely.

Generate the manifest once (any machine with credentials):

```bash
azcp ls https://acct.blob.core.windows.net/ctr/models/llama/ \
  --recursive --machine-readable > llama.shardlist
```

The output is plain TSV (`<name>\t<size>\t<last-modified>` per line); you
can ship it alongside the model, check it into git, regenerate it on a
schedule, or hand-edit it to subset the transfer.

Then consume on every node / process:

```bash
for i in 0 1 2 3; do
  azcp copy https://acct.blob.core.windows.net/ctr/models/llama/ /mnt/nvme/llama/ \
    --recursive --shardlist llama.shardlist --shard $i/4 \
    --concurrency 32 --block-size 16777216 &
done
wait
```

Notes:

- **Trust the file.** `azcp` does not re-validate the manifest against the live
  container. If a blob has been deleted, that file's GET fails with `404`
  and counts as a per-file failure (the rest of the transfer continues). If
  a blob has been resized, the GET still streams what's there — only the
  pre-computed total bytes for the progress bar will be off.
- **`--include-pattern` / `--exclude-pattern` still apply** on top of the
  shardlist, after parsing it. Use them to subset without regenerating.
- **`--shard` still applies** on top of the shardlist — that's the whole
  point. Each process reads the same file, runs the same LPT split, and
  takes its slice.
- **Download-only.** `--shardlist` on uploads or local→local copies is
  rejected with a clear error; for uploads, walking the local filesystem
  is already cheap.
- **Comment lines** starting with `#` and blank lines are skipped, so you
  can annotate the file freely. `<DIR>` rollup rows from non-recursive
  `azcp ls` output are also skipped automatically.

### Throttling and retries

Every operation retries `503 ServerBusy`, `429 Too Many Requests`, and transient
`5xx` responses with exponential backoff + jitter (honoring `Retry-After`). The
retry budget is controlled by `--max-retries` (default 5, env `AZCP_MAX_RETRIES`).

When progress display is active, the total bar suffix shows live retry counters, e.g.
`[retry 503x12 429x2]`, so you can tell at a glance that Azure is pushing
back. If uploads ultimately fail after exhausting retries, `azcp` exits with
a non-zero status and prints a `Failed: N` summary.

On very high-bandwidth endpoints you can raise `--concurrency` aggressively,
but if you see throttle counts climb, Azure is telling you you've hit the
account ingress/egress limit (typically ~60 Gbps ingress for standard storage;
egress caps observed around ~230 Gbps before hard 503 storms). Lower
concurrency, spread across multiple accounts, or request a quota increase.

### Capping bandwidth

`--max-bandwidth RATE` caps aggregate throughput across all workers (single
process — for multi-process / multi-node, see [docs/cluster.md](docs/cluster.md)).
Useful when sharing a NIC with other tenants or staying under a storage account
quota. Accepts both bit-rate and byte-rate units:

```bash
azcp copy https://acct.blob.core.windows.net/ctr/ ./dst/ \
  --recursive --workers 4 --max-bandwidth 50Gbps

azcp copy ./big https://acct.blob.core.windows.net/ctr/ \
  --recursive --max-bandwidth 500MiB/s
```

Accepted units: `Tbps`, `Gbps`, `Mbps`, `Kbps`, `bps` (bits/sec, decimal); `TB/s`,
`GB/s`, `MB/s`, `KB/s` (bytes/sec, decimal); `TiB/s`, `GiB/s`, `MiB/s`, `KiB/s`
(bytes/sec, binary). The trailing `/s` is optional. Bare numbers are bytes/sec.

The cap is a token bucket sized to one period: in steady state actual
throughput tracks the target within ~10% over multi-second windows. The
limiter starts empty (no initial burst), so per-request latency for the
very first request includes whatever wait is needed to mint enough tokens
for that block.

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
  --recursive --include-pattern '*.log'
```

### mk

```bash
azcp mk https://acct.blob.core.windows.net/new-container
```

## High-bandwidth NICs and multi-node broadcast

Two layers of additional throughput live in [`docs/`](docs/):

- **[docs/performance-tuning.md](docs/performance-tuning.md)** — single-node
  techniques for pushing 100-200+ Gbps NICs to line rate on bare VMs and
  AKS: `hostNetwork`, NUMA pinning, multi-process sharding, the AKS
  overlay-network bottleneck, and the storage-account egress ceiling.

- **[docs/cluster.md](docs/cluster.md)** — `azcp-cluster`, the companion
  MPI binary that downloads `1/N` of a dataset per rank and broadcasts it
  over RDMA/InfiniBand to every other rank. For the "many nodes, same
  dataset" pattern (model checkpoints across a training cluster), it pays
  Azure egress once and ends up 5-10× faster end-to-end than per-node
  downloads.
  - [docs/cluster-aks.md](docs/cluster-aks.md) — AKS deployment via
    mpi-operator (recommended) or StatefulSet (fallback).
    Examples: [examples/aks/](examples/aks/).
  - [docs/cluster-slurm.md](docs/cluster-slurm.md) — Slurm deployment via
    enroot + pyxis (recommended) or Apptainer (alternative).
    Examples: [examples/slurm/](examples/slurm/).
  - [docs/cluster-benchmarks.md](docs/cluster-benchmarks.md) — measured
    bcast and download bandwidth on a 16-node InfiniBand reference
    cluster.
  - [docs/cluster-h100-tuning.md](docs/cluster-h100-tuning.md) — Azure
    ND H100 v5 bring-up notes; hcoll, bcast-algorithm, and NUMA
    tuning recipes when 73 Gb/s default bcast isn't enough.

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

### `azcp-cluster` smoke tests

- [`tests/cluster_smoke.sh`](tests/cluster_smoke.sh) — single-node CLI smoke test (no MPI).
- [`tests/AKS_TESTING.md`](tests/AKS_TESTING.md) — end-to-end AKS smoke procedure.

## Continuous Integration

`.github/workflows/build.yml` builds all six platform targets on every push/PR using native runners (no cross-compilation). `.github/workflows/cluster-image.yml` builds and publishes the multi-arch `azcp-cluster` container to GHCR. Tag a release to publish both:

```bash
git tag v0.4.0
git push origin v0.4.0
```

Binaries land in the GitHub Release; the container lands at
`ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.0` and `:latest`.

## Project layout

```
src/                          azcp CLI (single-binary)
  auth/                       SharedKey, SAS, Bearer, Azure CLI credentials
  cli/                        Command definitions and argument parsing
  engine/                     Transfer engine: parallel scheduler, progress, glob filtering
  storage/blob/               Blob REST client (list, put block, get range, delete, ...)
  storage/local.rs            Local filesystem walk
crates/azcp-cluster/          Companion MPI binary for multi-node broadcast
docs/                         Long-form documentation
examples/                     Copy-paste deployment manifests (AKS, Slurm)
tests/                        Integration tests + smoke procedures
.github/workflows/            CI: per-platform binary build + cluster container build
```

## License

See `LICENSE`.
