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
  --recursive --concurrency 64 --block-size 8388608

# Filter with globs
azcp copy ./src https://acct.blob.core.windows.net/ctr/backup/ \
  --recursive --include-pattern '*.rs' --exclude-pattern 'target/*'
```

Flags: `--recursive`, `--no-overwrite`, `--block-size`, `--concurrency`, `--parallel-files`, `--workers`, `--shard`, `--shardlist`, `--max-retries`, `--dry-run`, `--check-md5`, `--include-pattern`, `--exclude-pattern`, `--progress`, `--no-progress`.

> Progress display is **on by default when stderr is a TTY** and silenced
> automatically when output is redirected (logs, CI, `kubectl logs`). Pass
> `--progress` to force it on (e.g. allocated PTY in CI, or `nohup` runs you
> intend to `tail -f`) and `--no-progress` to force it off.

### Throughput tuning

Two independent knobs control parallelism:

- `--workers N` (default 1): spawn N independent tokio runtimes in one process, each with its own `reqwest` connection pool and its own shard of the workload. **Required to scale past ~28 Gbps** — a single tokio runtime + reqwest client tops out there regardless of how many concurrent requests you submit (see "why workers are necessary" below).
- `--concurrency N` (default 64) and `--parallel-files N` (default 16, env `AZCP_PARALLEL_FILES`): per-runtime limits on in-flight HTTP block requests and actively-transferring files. With `--workers > 1`, these are per-worker.

For 16+ large files at 100+ GbE, `--workers 4 --concurrency 32 --parallel-files 4 --block-size 16777216` reaches **~64 Gbps** sustained on download from inside a default Kubernetes pod; `--workers 8` peaks at **~69 Gbps**. To go past that on faster NICs (100-200+ GbE), see [Performance tuning for high-bandwidth NICs](#performance-tuning-for-high-bandwidth-nics) — the limiting factor at that point is the host network stack and NUMA placement, not `azcp`'s tuning knobs.

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

The two flags **do not compose**: passing `--shard` alongside `--workers > 1`
prints a warning and the outer `--shard` is ignored (workers do their own
internal LPT split, so the user-supplied shard would conflict). For
multi-node deployments, run one process per node with `--shard $NODE/$NODES`
and leave `--workers` at 1, or scale `--workers` within each node and
partition across nodes by some other means (e.g. different prefixes per
node).

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

When progress display is active, the total bar suffix shows live retry counters, e.g.
`[retry 503x12 429x2]`, so you can tell at a glance that Azure is pushing
back. If uploads ultimately fail after exhausting retries, `azcp` exits with
a non-zero status and prints a `Failed: N` summary.

On very high-bandwidth endpoints you can raise `--concurrency` aggressively,
but if you see throttle counts climb, Azure is telling you you've hit the
account ingress/egress limit (typically ~60 Gbps ingress for standard storage;
egress caps observed around ~230 Gbps before hard 503 storms). Lower
concurrency, spread across multiple accounts, or request a quota increase.

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

## Performance tuning for high-bandwidth NICs

`azcp`'s default flags saturate a 25-50 GbE link with ease. Past that, the bottleneck stops being the wire and starts being the host's network stack, the kernel's per-CPU softirq queues, and (on container platforms) the overlay network sitting between your process and the NIC. This section documents the layers you have to peel back to drive a **100-200+ Gbps** NIC at line rate, and the recipes for both bare VMs and Kubernetes/AKS.

You only need this if:

- Your NIC is fast enough that one process can't saturate it (typically **100 GbE and up**).
- Your host has multiple NUMA nodes (most modern dual-socket or large single-socket servers).
- You are seeing wire utilization well below NIC capacity despite plenty of `--workers` and `--concurrency`.

On 10/25 GbE the techniques here still apply, but the gains are small — `--workers` alone is usually enough.

### Bottleneck progression (measured)

The following was measured downloading a 385 GiB / 175-file checkpoint from a single Azure storage account to a single node with a 200 Gbps NIC. Each row removes one bottleneck:

| Configuration | `azcp` flags (per process) | Procs | App Gbps | Wire Gbps | NIC util |
|---|---|---:|---:|---:|---:|
| Kubernetes overlay pod (default) | `--workers 8 --concurrency 32 --block-size 16777216` | 1 | 62.7 | ~70 | 35% |
| Pod with `hostNetwork: true` | `--workers 8 --concurrency 64 --block-size 16777216` | 1 | 117 | 127 | 64% |
| `hostNetwork` + NUMA-pinned (single proc) | `numactl --cpunodebind=N --membind=N azcp ... --workers 8 --concurrency 64 --block-size 16777216` | 1 | 131 | 145 | 73% |
| `hostNetwork` + NUMA-pinned, 2 procs | `numactl ... azcp ... --workers 1 --concurrency 32 --shard i/2 --block-size 16777216` | 2 | 150 | 158 | 79% |
| `hostNetwork` + NUMA-pinned, **4 procs** | `numactl ... azcp ... --workers 1 --concurrency 32 --shard i/4 --block-size 16777216` | **4** | **170** | **186** | **93%** |

All rows used the same 385 GiB / 175-file dataset (~2.2 GiB avg file size) with `--recursive`. `--parallel-files` was left at its default (`min(concurrency, file_count)`). The single-proc rows benefit from higher per-runtime concurrency (`--concurrency 64`); the multi-proc rows split work via `--shard` and use lower per-process concurrency since aggregate in-flight requests = `procs × concurrency`.

Net: **+103 Gbps (2.7×)** by combining `hostNetwork` (or running on bare metal/VMs), NUMA pinning, and multiple cooperating processes.

### NUMA — what it is and why it matters

Modern multi-socket and many-core servers split CPUs and memory into **NUMA nodes** (Non-Uniform Memory Access). Each NUMA node has its own bank of RAM and its own slice of the PCIe bus. Memory on the *local* node is fast to reach; memory on a *remote* node goes over an inter-socket link and is slower.

Your NIC is physically attached to *one* NUMA node. When packets arrive, the kernel's softirq runs on a CPU near the NIC and writes data into RAM. If your `azcp` process happens to be running on the *other* NUMA node, every received byte gets copied across the inter-socket link before your code can touch it. On a 200 Gbps NIC this costs ~10-15 Gbps and noticeably increases tail latency.

Find your NIC's NUMA node:

```bash
# Replace eth0 with your interface name (see `ip link`)
cat /sys/class/net/eth0/device/numa_node
# 1   ← NIC is on NUMA node 1
```

Then pin `azcp` to the same node using `numactl` (install via `apt install numactl` / `dnf install numactl`):

```bash
numactl --cpunodebind=1 --membind=1 azcp copy ...
```

`--cpunodebind` restricts threads to CPUs on that node; `--membind` restricts allocations to that node's memory. Both matter.

A single NUMA-pinned process won't saturate a fast NIC by itself — see the next section.

### Multi-process for line rate

Even with `hostNetwork` and NUMA pinning, a single `azcp` process tops out around 130 Gbps because tokio's scheduler and `reqwest`'s connection pool serialize work that no amount of internal parallelism can spread. To go further, run **multiple cooperating processes**, each pinned to the NIC's NUMA node, each handling a deterministic shard of the file set:

```bash
# 4 cooperating processes, all on NUMA node 1, each owning 1/4 of the workload
for i in 0 1 2 3; do
  numactl --cpunodebind=1 --membind=1 \
    azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
      --recursive --shard $i/4 --workers 1 --concurrency 32 \
      --block-size 16777216 &
done
wait
```

`--shard i/N` makes process `i` of `N` deterministically own its slice (size-balanced LPT bin packing — see [Throughput tuning](#throughput-tuning) above). Sharding works the same whether the `N` processes run on one node or across many.

### Bare VM recipe

```bash
# 1. Find your NIC's NUMA node
NUMA=$(cat /sys/class/net/eth0/device/numa_node)

# 2. Single high-throughput invocation (good up to ~130 Gbps single-process)
numactl --cpunodebind=$NUMA --membind=$NUMA \
  azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
    --recursive --workers 8 --concurrency 32 --block-size 16777216

# 3. Or fan out to multiple processes for max throughput
for i in 0 1 2 3; do
  numactl --cpunodebind=$NUMA --membind=$NUMA \
    azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
      --recursive --shard $i/4 --workers 1 --concurrency 32 \
      --block-size 16777216 &
done
wait
```

### Kubernetes / AKS recipe

The default pod network on most Kubernetes clusters (including AKS Azure CNI overlay) puts every packet through a `veth` pair, a bridge, and (on overlay) a VXLAN tunnel. The pod's `veth` typically has a single RX queue with RPS (Receive Packet Steering) disabled, so all inbound network softirq for the pod lands on **one CPU**. That CPU saturates around 50-70 Gbps regardless of how many workers `azcp` is using.

`hostNetwork: true` bypasses the overlay entirely — your process uses the host's NIC directly, with all of its hardware queues and softirq spread across many cores.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: azcp-fast
spec:
  hostNetwork: true
  dnsPolicy: ClusterFirstWithHostNet
  containers:
  - name: azcp
    image: your/image-with-azcp-and-numactl
    command: ["bash", "-lc"]
    args:
    - |
      set -euo pipefail
      apt-get update && apt-get install -y numactl
      NUMA=$(cat /sys/class/net/eth0/device/numa_node)
      for i in 0 1 2 3; do
        numactl --cpunodebind=$NUMA --membind=$NUMA \
          azcp copy https://acct.blob.core.windows.net/ctr/prefix/ /dst/ \
            --recursive --shard $i/4 --workers 1 --concurrency 32 \
            --block-size 16777216 &
      done
      wait
```

`hostNetwork: true` is the single biggest win on AKS — typically **+50 Gbps** over the overlay, before any other tuning. It does mean the pod shares the host's network namespace, so coordinate port usage with anything else running on the node.

### Account-level ceiling

Once a single node is doing 170-200 Gbps, the next ceiling you'll hit is the **storage account egress cap** itself (~230 Gbps observed for a standard general-purpose v2 account, with hard 503 `ServerBusy` storms above that). Scale across multiple accounts or request a quota increase if you need more.

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
