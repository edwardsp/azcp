# `azcp-cluster` benchmarks

Reference numbers for `azcp-cluster` on a 16-node InfiniBand cluster, plus
the methodology for reproducing them on your own hardware.

## What the numbers mean

Every run prints a six-line summary on rank 0. Two lines matter:

- **`[download] BW=`** — aggregate Azure throughput, summed across ranks.
  Bounded by per-node Azure NIC × ranks, then by storage-account egress
  (~230 Gb/s on a single GPv2 account).
- **`[bcast] BW=`** — per-receiver throughput. Every non-owner rank
  received this much per second. Bounded by fabric, MPI/UCX overhead,
  destination filesystem write speed, and (until you opt into them) the
  defaults below.

`[bcast] > [download]` proves the fabric path beat per-node Azure egress,
which is the whole point of the tool.

## Tuning options

| Flag | Default | What it does |
|---|---|---|
| `--shard-size` | `0` | Range-shard each file into byte ranges of this size before assigning owners. `0` = one shard per file (legacy). With **fewer files than ranks** (typical of LLM checkpoints) this multiplies bcast bandwidth by the number of broadcasters per file. See [cluster-v0.4-shard-size-sweep.md](cluster-v0.4-shard-size-sweep.md) for a 3× win on 16× ND H100 v5. Must be a multiple of `--bcast-chunk`. |
| `--file-shards` | (off) | Convenience: derive `--shard-size` as `ceil(max_file_size / N)` chunk-aligned. Mutex with `--shard-size`. |
| `--bcast-chunk` | `64M` | MPI message size per `MPI_Ibcast`. Larger → fewer collectives, deeper pipeline buffer. |
| `--bcast-pipeline` | `1` | Number of in-flight `MPI_Ibcast`s per file. Larger → better fabric overlap, more memory. Defaults to 1 (TCP-friendly); raise to 4-8 on RDMA. |
| `--bcast-writers` | `2` | Writer threads per rank for received chunks. `1` serializes writes; `2+` parallelises across the pipeline. With `--shard-size > 0`, often want 4-8. |
| `--bcast-direct` | `true` | Open destination files with `O_DIRECT` (4 KiB-aligned, bypasses page cache). On NVMe, lifts single-thread write ceiling from ~47 Gb/s to ~100 Gb/s. Disable with `--no-bcast-direct`. |
| `--verify` | `off` | After bcast, MD5 each file on every rank, MPI-Allgather, fail if any rank disagrees. Adds ~25-30 s on this dataset. |
| `--compare` | `size` | Skip files already present at the right size. `none` forces every byte to transfer. |

For NDR-class fabrics (≥200 Gb/s per HCA), set
`--bcast-chunk 512M --bcast-pipeline 16`. Defaults are tuned conservatively
for portability.

## Reference results

For ND H100 v5 — different fabric, different defaults, different
bottleneck — see the dedicated tuning notes in
[cluster-h100-tuning.md](cluster-h100-tuning.md).

### Test environment

- **Cluster**: 16 × Azure GB300 nodes, InfiniBand interconnect
  (NDR 400 Gb/s, 4 × `mlx5_*:1` per node, single-rail use).
- **Source**: Azure Blob Storage, GPv2 account, same region. Dataset
  `nvidia/DeepSeek-R1-0528-NVFP4-v2`: 524 files, 413 GiB
  (~788 MiB average file size).
- **Destination**: per-node NVMe at `/mnt/nvme/dataset` (4-way RAID-0 of
  `Microsoft NVMe Direct Disk v2`, ext4, `noatime,discard,stripe=512`).
  `tmpfs` rows use `emptyDir: {medium: Memory}` with 720 GiB pod limit.
- **Software**: `azcp-cluster` ≥ v0.4.2 container, Open MPI 4.x + UCX
  bundled. Run via StatefulSet on AKS with `hostNetwork: true`,
  `IPC_LOCK`, `rdma/ib: 4`.
- **mpirun env (RDMA)**:
  ```
  -mca pml ucx -mca osc ucx
  -x UCX_TLS=rc,sm,self
  -x UCX_NET_DEVICES=mlx5_0:1,mlx5_1:1,mlx5_2:1,mlx5_3:1
  -x UCX_IB_GID_INDEX=3
  ```
- **mpirun env (TCP baseline)**:
  ```
  -mca pml ob1 -mca btl tcp,self -mca btl_tcp_if_include eth0
  ```
- All bcast runs use `--bcast-chunk 512M --bcast-pipeline 16
  --concurrency 32 --block-size 16777216 --compare size`.

### Single-run results (REPS=1)

| Transport | Destination | `--bcast-writers` | `--bcast-direct` | `[download]` Gb/s | `[bcast]` Gb/s | `[total]` |
|---|---|---:|:---:|---:|---:|---:|
| RDMA | NVMe  | 1 | no  | 192.0 |  28.4 | 133.7 s |
| RDMA | NVMe  | 2 | yes | 236.0 | **99.2** | **47.7 s** |
| RDMA | NVMe  | 8 | yes | 278.7 |  99.1 |  45.6 s |
| RDMA | tmpfs | 2 | n/a | 278.0 | 139.5 |  35.8 s |
| TCP  | NVMe  | 2 | yes | 206.3 |  12.1 | 289.1 s |

**Read this as:**

- **Row 2 is the shipped default** on this hardware. 99 Gb/s per receiver,
  413 GiB delivered to all 16 nodes in 48 s.
- **Row 1** is what you'd get with `--bcast-writers 1 --no-bcast-direct`:
  buffered single-threaded writes to ext4 cap the path at ~28 Gb/s
  regardless of fabric speed. This is the regression mode the v0.3.0
  defaults exist to avoid.
- **Row 3** confirms 2 writers already saturates the path; 4× more
  writers buys nothing on this disk array.
- **Row 4** is the fabric-only ceiling (no disk in the path). The 40
  Gb/s gap between rows 2 and 4 is unrecovered NVMe write overhead;
  reaching it would need either a faster array or a fundamentally
  different write strategy.
- **Row 5** is the no-IB fallback. TCP collapses to ~12 Gb/s. At that
  throughput per-node `azcp --shard` over Azure (~14-18 Gb/s per node)
  is competitive — `azcp-cluster` still wins on egress cost (1× vs N×
  account egress) but no longer on wall-clock.

Single-run measurements swing 5-10% across reruns. For tighter numbers
re-run the sweep with `REPS=3` and report the median.

## Reproducing

The script [`tests/cluster_bench.sh`](../tests/cluster_bench.sh) renders
the StatefulSet, runs `mpirun`, captures rank-0 logs, and prints a
markdown table. Required env:

| Variable | Default | Notes |
|---|---|---|
| `SOURCE_URL` | — | Azure blob prefix to broadcast. Use a realistic dataset (≥100 GiB, otherwise warm-up dominates). |
| `IMAGE` | — | `azcp-cluster` container image. |
| `AZURE_CLIENT_ID` | — | Workload-identity client ID for source-side auth. |
| `NODEPOOL` | — | AKS node pool label (`agentpool=...`). |
| `REPLICAS` | — | Number of MPI ranks (one pod per node, `podAntiAffinity` enforced). |
| `CONFIGS` | — | Semicolon-separated `label;chunk;pipeline;ucx_tls`. Multiple values run in sequence. |
| `BCAST_EXTRA_ARGS` | empty | Extra flags forwarded to `azcp-cluster`. Use this to set `--bcast-writers`, `--bcast-direct`, `--verify`. |
| `DEST` | `/mnt/nvme/dataset` | Per-pod destination path. Set to `/dev/shm/dataset` for tmpfs runs. |
| `SHM_SIZE` | unset | `emptyDir.sizeLimit` for `/dev/shm`. Required for tmpfs. |
| `MEM_LIMIT` | `64Gi` | Pod memory limit. Must exceed dataset size for tmpfs runs. |
| `REPS` | `3` | Repetitions per config; the script reports the median. |
| `COMPARE` | `size` | Forwarded to `--compare`. Use `none` for cold-cache measurement. |
| `KUBECONTEXT` | current | kubectl context. |

Example (the row-2 default above):

```bash
SOURCE_URL='https://acct.blob.core.windows.net/models/llama/' \
IMAGE='ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.2' \
AZURE_CLIENT_ID='<workload-identity-client-id>' \
NODEPOOL=gb300 REPLICAS=16 REPS=3 \
CONFIGS='rdma-512m-pipe16;536870912;16;rc,sm,self' \
BCAST_EXTRA_ARGS='--bcast-writers 2 --bcast-direct' \
  bash tests/cluster_bench.sh
```

The benchmark is intentionally **not** part of CI: it needs real hardware
(InfiniBand, NVMe, multi-node), a real dataset, and human judgment to
interpret. Run it pre-release and when bringing up a new cluster type.

## Tool-choice rules of thumb

- **`[bcast] > 2 × per-node Azure throughput`** → `azcp-cluster` is
  faster *and* cheaper than per-node `azcp --shard`. Use it.
- **`[bcast] ≈ per-node Azure throughput`** (e.g. TCP-only fabrics) →
  `azcp-cluster` still saves N× account egress but `azcp --shard` may
  finish faster wall-clock. Pick on cost vs latency.
- **Single-node deployment** → don't use `azcp-cluster` at all. Use
  `azcp` with `--workers` and (on 100+ GbE NICs) the techniques in
  [performance-tuning.md](performance-tuning.md).

## Future work

- **Multi-rail UCX bcast.** Single-rail caps row 4 at ~140 Gb/s; the
  GB300 nodes have 4 HCAs that could push this further.
- **Bcast tuning autodetect.** Probe fabric at startup, pick
  chunk/pipeline accordingly instead of shipping conservative defaults.
- **Multi-account source.** At very high rank counts the single-account
  egress ceiling dominates `[download]`.
- **AMD MI300X cluster numbers.** Different fabric, different tuning.
