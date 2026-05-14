# `azcp-cluster` — multi-node broadcast download

`azcp-cluster` is a companion MPI binary to `azcp` for the case where **many
nodes need the same dataset** (distributed training pulling the same
checkpoint, batch jobs that all read the same reference data, etc.).

It pays Azure egress **once per byte** instead of once per node, and uses the
cluster's interconnect (RDMA/InfiniBand if present, TCP otherwise) to fan the
data out to every rank at fabric speed.

## Why a separate binary

`azcp` is a regular CLI: one process, one (or many) tokio runtimes, one
node's worth of NIC. If you have 16 nodes and run `azcp` 16 times against
the same prefix, you do `16 ×` the LIST work and pay `16 ×` the Azure
egress. On a fast cluster fabric (200 Gbps IB, etc.) that's wasteful:
intra-cluster bandwidth is much higher than per-node Azure throughput, and
egress quotas are real money.

`azcp-cluster` is the same engine wrapped in MPI:

1. Rank 0 lists the source once and broadcasts the file list.
2. The N ranks deterministically partition the list with the same
   size-balanced LPT bin packing `azcp --shard` uses, so each rank pulls
   only `1/N` of the bytes from Azure in parallel.
3. After download, every file is `MPI_Bcast`'d from its owner rank to all
   other ranks. UCX picks RDMA when it's available; otherwise TCP.

For 10+ nodes on a fast fabric, end-to-end time drops by **5-10×** vs the
naive "every node downloads from Azure" approach. Measured numbers are in
[cluster-benchmarks.md](cluster-benchmarks.md).

## Distribution

`azcp-cluster` is shipped **as a container only**. It links Open MPI 4.x and
dlopens UCX, libibverbs, libmlx5, and various rdma-core providers at runtime;
shipping a self-contained binary that works against arbitrary host MPI/UCX
stacks is a portability minefield we are not paying for. The container
covers the two consumption models we care about:

- **Kubernetes / AKS**: pull the image directly. See
  [cluster-aks.md](cluster-aks.md) for the mpi-operator setup.
- **Slurm**: import the same OCI image with `enroot` (or convert with
  `apptainer build`) and run under `srun --container-image=`. See
  [cluster-slurm.md](cluster-slurm.md).

Image:

```
ghcr.io/edwardsp/azcp/azcp-cluster:<tag>
```

Multi-arch (`linux/amd64`, `linux/arm64`). Tags: `latest` and `vX.Y.Z` on
releases, `edge` on `main`, `<branch>-<sha>` for PRs.

## Requirements

- **One process per node.** The binary aborts with exit code 2 if it sees
  more than one rank sharing a node — sharding is per-node, not per-rank.
- **An MPI launcher.** The image carries Open MPI 4.x and UCX; on the host
  side, `mpirun` (kubectl-launched), `mpirun --hostfile` (bare Slurm), or
  the mpi-operator's `MPIJob` Launcher pod all work.
- **Local fast storage** on every node, mounted at the same path on all
  nodes. NVMe is recommended; the bcast stage is bandwidth-bound and slow
  destination disks become the bottleneck.
- **Storage credentials** on every rank. Workload identity (AKS) or a
  managed identity attached to each VM (Slurm-on-Azure) is preferred over
  SAS — long downloads outlive most SAS tokens.

For RDMA-capable runs, also:

- `IPC_LOCK` capability in the container (or the equivalent `ulimit -l
  unlimited` on bare metal).
- `/dev/shm` of at least a few GiB (use a `Memory` `emptyDir` on Kubernetes).
- An IB device exposed to the container — `rdma/ib: 4` resource on AKS GPU
  pools, or pyxis `--container-mounts=/dev/infiniband` on Slurm.

## CLI

```
azcp-cluster <SOURCE> <DEST> [flags]
```

| Flag | Default | Purpose |
|---|---|---|
| `--shardlist FILE` | (off) | Skip the LIST API; read pre-generated TSV manifest (same format as `azcp ls --machine-readable`). Saves the LIST round-trip on rank 0 for very large datasets. |
| `--compare {none,size,filelist}` | `none` | Skip-policy. `none` re-transfers every file. `size` skips files where the local size matches the blob size. `filelist` (requires `--filelist FILE`) skips when local size **and** the blob's `Last-Modified` both match the prior-run filelist. |
| `--filelist FILE` | — | Required when `--compare filelist`. Prior-run TSV with sizes + `Last-Modified`. |
| `--save-filelist FILE` | — | After the run, rank 0 writes the post-run TSV to `FILE` for use as the next run's `--filelist`. |
| `--concurrency N` | 64 | In-flight HTTP block requests per rank. |
| `--block-size N` | 16 MiB | Block size for downloads. |
| `--parallel-files N` | 16 | Files actively downloading concurrently per rank. |
| `--bcast-chunk N` | 64 MiB | Chunk size for `MPI_Bcast`. Tune for fabric — see [Tuning](#tuning) below. |
| `--bcast-pipeline N` | 1 | Number of bcast chunks in flight per file. Higher = more pipelining, more memory (`bcast_pipeline * bcast_chunk` per rank). Defaults to 1; raise on RDMA / multi-rail where extra concurrency actually finds more bandwidth. |
| `--bcast-writers N` | 2 | Receiver-side writer threads. Single-threaded buffered writes can bottleneck below NVMe line rate; fanning across multiple threads (dispatched by `file_id % N`) restores throughput. Owner ranks unaffected. |
| `--no-bcast-direct` | — | Disable `O_DIRECT` for bcast output files. By default writes bypass the page cache, lifting single-thread NVMe write throughput from ~47 → ~100 Gb/s. Set this on filesystems that don't support `O_DIRECT` (NFS, some FUSE mounts). |
| `--shard-size N` | 0 | Range-shard each file into byte chunks of this size before distributing across ranks. `0` = one shard per file (legacy v0.3.1 plan shape). When `>0`, must be a multiple of `--bcast-chunk`. Accepts byte sizes with units: `32GiB`, `1GB`, `16777216`. Mutually exclusive with `--file-shards`. See [Range sharding](#range-sharding) below. |
| `--file-shards N` | (off) | Convenience knob: derive `--shard-size` as `ceil(max_file_size / N)` rounded up to a multiple of `--bcast-chunk`. Mutually exclusive with `--shard-size`. |
| `--max-retries N` | 5 | Per-HTTP-request retry budget. |
| `--max-bandwidth RATE` | (off) | Cap **total cluster** download throughput. Accepts bit/byte units (`50Gbps`, `1GB/s`, `500MiB/s`). Divided across active downloader ranks: each gets `RATE / K`. See [Throughput limits](#throughput-limits) below. |
| `--download-ranks K` | (= world size) | Only ranks `0..K-1` download from Azure; the remaining ranks skip the download phase and only receive via `MPI_Bcast`. Use to avoid 503 throttling when too many ranks hit one storage account. See [Limiting downloader count](#limiting-downloader-count) below. |
| `--verify` | — | After bcast, every rank MD5s its local copy of every file and MPI-Allgathers the digests; mismatch across ranks (or vs. `Content-MD5` when present) exits non-zero. Adds ~25-30 s on a 1.2 TB / 6-file workload. |
| `--no-progress` | — | Disable per-rank progress bars (default: TTY-aware). |

## Stages and timing output

`azcp-cluster` runs in five stages. Rank 0 prints exactly six summary lines
after a successful run:

```
[list]     <files> files, <bytes> bytes T=<sec>s
[diff]     <to-transfer> to transfer, <skipped> skipped T=<sec>s
[download] <bytes> bytes T=<sec>s BW=<aggregate>
[bcast]    <files> files <bytes> bytes T=<sec>s BW=<per-receiver>
[filelist] (wrote <n> entries | skipped (no --save-filelist)) T=<sec>s
[total]    T=<sec>s
```

`[shard]` lines on stderr show per-rank slice sizes (one per rank, useful
for debugging LPT balance).

The contract on those six lines is stable — scripts and benchmarks parse
them. The aggregate `BW` on `[download]` is summed across all ranks; the
per-receiver `BW` on `[bcast]` is what every non-owner rank actually
received. A healthy run has `[bcast] BW > [download] BW`: that's the proof
that the fabric path beat per-node Azure egress.

## Failure semantics

- LIST or any per-rank stage error → `MPI_Abort` with a non-zero exit code
  (3 = list, 4 = diff, 5 = download, 6 = bcast). The whole job exits.
- Multiple ranks per node detected → exit code 2 with an explanatory
  message. (Run with `mpirun -N 1` / one task per node.)
- Per-file download failures inside a rank's shard surface as a non-zero
  exit via the engine's `Failed: N` summary; the broadcast stage will
  never run if any rank reports a failed download.

## Tuning

Defaults are reasonable for 8-32 nodes on a 100-200 GbE fabric. The knobs
that matter for bcast performance:

- `--shard-size` / `--file-shards` — **the biggest lever in v0.4.** With
  large files (10+ GiB) and few of them (≤ N ranks), the legacy "one
  broadcaster per file" plan leaves most ranks idle during bcast.
  Range-sharding splits each file into multiple owners that broadcast in
  parallel. On a 16-node ND H100 v5 cluster with 1.2 TB / 6 files, this
  lifts bcast bandwidth from ~44 Gb/s (legacy) to **~134 Gb/s** at
  `--shard-size 8GiB` — a 3× win. See [Range sharding](#range-sharding)
  below.
- `--bcast-chunk` — too small and you pay MPI per-message overhead per
  chunk; too large and you serialize the bcast (no overlap with the next
  chunk). 64 MiB is the default. On RDMA fabrics with deep queues we've
  measured improvements at 256 MiB; on TCP, smaller chunks (16 MiB) are
  often better.
- `--bcast-pipeline` — number of chunks per file in flight. Defaults to 1
  (TCP-friendly; OpenMPI's MPI_Bcast already pipelines internally). On
  RDMA fabrics raise to 4-8.
- `--bcast-writers` — receiver write threads. Defaults to 2; on NVMe with
  `--shard-size > 0` you'll often want 4-8 to keep up with multiple
  in-flight broadcasters per node.

For first-run tuning, start with `--shard-size 8GiB` (if your files are
≥ 16 GiB) and watch the `[bcast] BW` line. See
[cluster-benchmarks.md](cluster-benchmarks.md) and
[cluster-h100-tuning.md](cluster-h100-tuning.md) for empirical sweeps.

### Range sharding

By default each file has exactly one broadcaster (the rank that
downloaded it). For workloads with **fewer files than ranks** (typical of
LLM checkpoints: a handful of multi-GB shard files), this caps bcast
bandwidth at "what one sender + one receiver pair can push through the
fabric" — usually 40-50 Gb/s on a 200 GbE NDR rail.

Range sharding (`--shard-size N`) cuts each file into byte ranges of
size `N` and assigns each range to a different owner rank via the same
size-balanced LPT bin packing used for files. Multiple owners now
broadcast slices of the same file in parallel, multiplying bcast
bandwidth by the number of concurrent broadcasters.

Measured on 16× ND H100 v5, 1.2 TB / 6 files (single rail):

| `--shard-size` | shards | DL Gb/s | Bcast Gb/s | speedup vs legacy |
|---|---|---|---|---|
| `0` (legacy) | 6 | 137 | 43.8 | 1.00× |
| `32GiB` | 42 | 197 | 114.5 | 2.61× |
| `16GiB` | 78 | 243 | 132.3 | 3.02× |
| `8GiB` | 150 | 227 | 134.0 | 3.06× |
| `2GiB` | 600 | 237 | 137.7 | 3.14× |

The sweet spot is **8-16 GiB**: smaller shards add coordination overhead
without buying more bandwidth, larger ones leave concurrency on the
table. `--file-shards N` is the convenience form: `N` is the desired
number of pieces of the largest file (e.g. `--file-shards 16` on a
200 GiB file → ~12.5 GiB shards, rounded up to a `--bcast-chunk`
multiple).

Constraints:

- `--shard-size % --bcast-chunk == 0` is enforced (so `O_DIRECT` padding
  never straddles shard boundaries; only each file's final chunk needs
  trim).
- `--shard-size` and `--file-shards` are mutually exclusive.
- `--shard-size 0` reproduces v0.3.1 behaviour exactly (validated against
  baseline: 43.8 Gb/s vs historical 42.5 Gb/s, within noise).

Range sharding also accelerates the **download** stage: more
independent units of work give the LIST→GET pipeline more to chew on
(137 → 243 Gb/s in the table above).

### `--bcast-chunk` and `--bcast-pipeline`

- `--bcast-chunk` — too small and you pay MPI per-message overhead per
  chunk; too large and you serialize the bcast (no overlap with the next
  chunk). 64 MiB is the default. On RDMA fabrics with deep queues we've
  measured improvements at 256 MiB; on TCP, smaller chunks (16 MiB) are
  often better.
- `--bcast-pipeline` — number of chunks per file in flight. 1 is the
  default. With RDMA, 4-8 has measurably better pipelining; on TCP 1-2 is
  usually right (more in-flight chunks just causes head-of-line blocking
  on the single TCP connection per pair).

### Throughput limits

`--max-bandwidth RATE` caps the cluster's **aggregate** download bandwidth.
Internally each downloader rank gets `RATE / K` (where K is the active
downloader count, see below). Accepts the same units as the single-process
flag: `50Gbps`, `200Mbps`, `1GB/s`, `500MiB/s`, etc.

```bash
mpirun ... azcp-cluster <src> <dst> --max-bandwidth 100Gbps
```

Common reasons to set this:

- **Quota-friendly**: stay under a known storage account egress ceiling
  (typically 60-230 Gbps depending on tier and account class) so 503s
  don't dominate the run.
- **Shared infrastructure**: cap so concurrent jobs on the same storage
  account or NIC don't collide.

The cap only applies to the **download** stage; `MPI_Bcast` over the
cluster fabric is not throttled.

### Limiting downloader count

`--download-ranks K` restricts the download phase to the first K ranks
(rank 0..K-1). The remaining ranks sit idle during download and only
participate in `MPI_Bcast`. Defaults to all ranks.

```bash
mpirun -np 64 ... azcp-cluster <src> <dst> --download-ranks 8
```

When to use it:

- **Throttling**: 64+ ranks hitting one storage account simultaneously
  often causes 503 storms. Restrict to 8-16 downloaders, then rely on
  bcast to fan out to the other ranks at fabric speed (which is far
  faster than re-trying throttled GETs anyway).
- **NIC fairness**: if some nodes have weaker uplinks or are noisy
  neighbours, exclude them from the download phase.

The split is deterministic: the LPT bin-packer divides the dataset into
K size-balanced shards (just as for K=N), and the broadcast plan is
computed from "who owns the file", so receiver ranks don't need to know
or care which subset is K. Combined with `--max-bandwidth`, each of the K
downloaders gets `total / K` bytes/sec.

## Skip-on-rerun (`--compare`)

For repeated transfers of the same dataset (CI, multi-job pipelines), use
`--save-filelist` on the first run and `--compare filelist --filelist` on
subsequent runs. Files whose size **and** `Last-Modified` match the prior
filelist are skipped at LIST speed — neither the Azure download nor the
intra-cluster bcast happens for them.

`--compare size` is cheaper to set up (no filelist needed) but less
precise: if a blob is replaced by another of the same size, `size` will
miss the change. For mutable datasets, prefer `filelist`.

`--compare none` (the default) re-transfers everything. Use it when you
want a cold-cache baseline measurement.

## See also

- [cluster-aks.md](cluster-aks.md) — AKS deployment via mpi-operator (primary)
  and StatefulSet (fallback).
- [cluster-slurm.md](cluster-slurm.md) — Slurm deployment via enroot
  (primary) and Apptainer (alt).
- [cluster-benchmarks.md](cluster-benchmarks.md) — measurement methodology
  and reference numbers.
- [cluster-v0.4-shard-size-sweep.md](cluster-v0.4-shard-size-sweep.md) —
  v0.4 `--shard-size` sweep on 16× ND H100 v5: 3× bcast bandwidth on
  large-checkpoint workloads.
- [cluster-h100-tuning.md](cluster-h100-tuning.md) — Azure ND H100 v5
  bring-up notes, hcoll/algorithm/NUMA tuning recipes.
- [performance-tuning.md](performance-tuning.md) — single-node tuning
  background. Most of it doesn't apply to `azcp-cluster` (which is
  fabric-bound, not host-network-stack-bound), but the NUMA section is
  useful when sizing pod CPU requests.
