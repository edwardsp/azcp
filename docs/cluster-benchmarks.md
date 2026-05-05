# `azcp-cluster` benchmarks

Reference numbers for `azcp-cluster` on a 16-node InfiniBand cluster, plus
the methodology for reproducing them on your own hardware.

## Why benchmark

The whole point of `azcp-cluster` is "intra-cluster bcast is faster than
per-node Azure egress." That claim is hardware-dependent — on a slow
fabric or a small cluster, plain `azcp --shard` per node may actually win.
The benchmark answers two questions for your specific setup:

1. **Is the bcast path faster than the Azure path?** (i.e. is
   `azcp-cluster` the right tool here at all?)
2. **What chunk-size / pipeline-depth maximizes bcast throughput?** Defaults
   are tuned conservatively; on NDR-class fabrics you generally want
   larger chunks and a deeper pipeline.

## Methodology

We measure the **6-line summary** that rank 0 prints. The two BW lines are
the scoreboard:

- `[download] BW=` — aggregate Azure throughput, summed across all ranks.
  Limited by per-node Azure NIC throughput × number of ranks (with
  account-egress as the next ceiling above ~230 Gbps).
- `[bcast] BW=` — per-receiver throughput. Every non-owner rank received
  this much per second. Limited by fabric throughput, MPI/UCX overhead,
  and (until v0.2.1) destination disk write speed.

A run with `[bcast] BW > [download] BW` proves the fabric path beat
per-node Azure egress, which is the whole point of the tool.

### Reproducing the benchmark

The script [`tests/cluster_bench.sh`](../tests/cluster_bench.sh) runs a
parameter sweep and prints a markdown results table. It expects:

- `$NODES` — comma-separated list of hostnames or rank counts (depends on
  launcher; the script supports both AKS via `kubectl` and Slurm via
  `sbatch`).
- `$SOURCE_URL` — Azure blob prefix to broadcast from. Should be a
  realistic dataset (~100 GiB minimum, otherwise warm-up dominates).
- `$DEST` — local destination path on every node (NVMe recommended).

Each `(chunk, pipeline)` configuration is run `REPS` times (default 3); the
median `[bcast] BW` is reported. Wall-clock time per run is `~1-8 minutes`
depending on dataset size, fabric, and version (v0.2.1 is ~3× faster on
NVMe than v0.2.0), so a full 12-cell sweep on 16 nodes takes 30 min - 2 h.

> The benchmark is intentionally **not** part of CI. It needs real
> hardware (InfiniBand, NVMe, multi-node), real datasets, and human
> judgment to interpret. Run it pre-release and when bringing up a new
> cluster type.

## Reference results

> Numbers below are from single benchmark runs (REPS=1) on the
> configuration documented under "Test environment." Re-run on your own
> hardware before using these as targets — fabric, kernel, UCX version,
> and storage account region all move the numbers. Single-run measurements
> can swing 5-10% across reruns.

### Test environment

- **Cluster**: 16 × Azure GB300 nodes, InfiniBand interconnect
  (NDR 400 Gb/s, 4 × `mlx5_*:1` per node).
- **Source**: Azure Blob Storage, standard general-purpose v2 account,
  same region as the cluster. Dataset: 524 files, 413 GB total
  (~788 MiB average file size).
- **Destination**: per-node NVMe at `/mnt/nvme/dataset` (4-way RAID-0
  array of `Microsoft NVMe Direct Disk v2`, ext4).
- **Software**: `azcp-cluster` v0.2.1 container, Open MPI 4.x + UCX
  bundled. Run via StatefulSet on AKS with `hostNetwork: true`,
  `IPC_LOCK`, `rdma/ib: 4`.
- **mpirun env (RDMA runs)**:
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

### v0.2.1 — Bcast configuration sweep on NVMe (current)

This is what you should expect on the released container. The async writer
introduced in v0.2.1 (see "How we got here" below) lifted the NVMe ceiling
roughly 3× across the board.

| Transport | `--bcast-chunk` | `--bcast-pipeline` | `[bcast]` Gb/s | `[total]` |
|---|---:|---:|---:|---:|
| RDMA | 64 MiB  | 4  | 68.77 | 76 s |
| RDMA | 64 MiB  | 2  | 53.54 | 92 s |
| RDMA | 256 MiB | 8  | 64.87 | 78 s |
| RDMA | 512 MiB | 16 | **76.27** | **64 s** |

**Top-line takeaway:** `--bcast-chunk 512M --bcast-pipeline 16` delivers
~76 Gb/s per receiver and finishes the 413 GB / 524-file transfer across
16 nodes in **64 seconds** wall-clock. The numbers asymptote near 70-76
Gb/s regardless of how aggressively we tune chunk/pipeline beyond that —
the next ceiling is single-rail UCX `MPI_Bcast` tree topology, well below
the 400 Gb/s theoretical NDR limit.

## How we got here (the diagnostic arc)

The v0.2.1 numbers above are what you get today. Everything below is the
sequence of measurements that explained why the tool was previously much
slower on NVMe — kept here because (a) it's useful context for anyone
debugging similar throughput problems on their own fabric, and (b) the
v0.2.0 numbers are a fair point of comparison if you're reading old
container images.

### v0.2.0 — Bcast configuration sweep on NVMe (historical)

| Transport | `--bcast-chunk` | `--bcast-pipeline` | `[bcast]` Gb/s | `[total]` |
|---|---:|---:|---:|---:|
| TCP (no IB)  | 64 MiB  | 4 | **7.64**  | 446 s |
| RDMA         | 64 MiB  | 4 | 18.13     | 194 s |
| RDMA         | 64 MiB  | 2 | 17.79     | 199 s |
| RDMA         | 256 MiB | 8 | **20.26** | 176 s |

In v0.2.0, RDMA was ~2.7× faster than TCP on this fabric (20.26 Gb/s vs
7.64 Gb/s per receiver) — but bcast asymptoted at ~20 Gb/s on NVMe
regardless of chunk/pipeline tuning. That asymptote was the thing that
needed explaining.

### Diagnostic A — destination on tmpfs (`/dev/shm`)

To isolate fabric and MPI performance from local-disk write throughput,
we re-ran the same configurations writing to a 640 GiB tmpfs
(`emptyDir: { medium: Memory }`, pod `memory: 720Gi`) instead of
`/mnt/nvme`, on **v0.2.0**:

| Transport | `--bcast-chunk` | `--bcast-pipeline` | `[bcast]` Gb/s | `[total]` |
|---|---:|---:|---:|---:|
| RDMA | 256 MiB | 8  | 54.68 | 72 s |
| RDMA | 256 MiB | 16 | 55.13 | 75 s |
| RDMA | 512 MiB | 8  | 55.21 | 73 s |
| RDMA | 512 MiB | 16 | **57.85** | **70 s** |

With the disk removed from the path, bcast bandwidth jumped **2.7×**
(20.26 → 57.85 Gb/s) and end-to-end wall-clock dropped to ~70 s for the
413 GB dataset. That's strong evidence the bottleneck involved the disk
write path — but as the next experiment shows, "involves the disk" is not
the same as "the disk is too slow."

### Diagnostic B — what the disk array can actually sustain

A natural assumption is that 20 Gb/s is the disk's sequential-write
ceiling. The GB300 SKU has a **4-way RAID-0 array** of `Microsoft NVMe
Direct Disk v2` drives (`md0`, 14 TB total, 512 KiB stripe, ext4 mounted
with `noatime,discard,stripe=512`). A standalone `fio` benchmark on
`/mnt/nvme` from a single pod measures the array's actual write
ceiling — and it's **far above** what v0.2.0 was delivering:

| `fio` config | `numjobs` | bandwidth |
|---|---:|---:|
| Buffered, `psync`, `bs=1M` | 1  | **40 Gb/s** |
| Buffered, `psync`, `bs=1M` | 4  | 76 Gb/s |
| Buffered, `psync`, `bs=1M` | 16 | 95 Gb/s |
| `O_DIRECT`, `libaio iodepth=32`, `bs=1M` | 1 | **105 Gb/s** |
| `O_DIRECT`, `libaio iodepth=32`, `bs=1M` | 4 | 104 Gb/s |

The disk array sustains **40-105 Gb/s** depending on access pattern.
v0.2.0 writing to that same array delivered 20 Gb/s — half of even the
worst single-stream buffered case. The disk wasn't the bottleneck; our
own code's interaction with it was.

### Root cause — synchronous writes on the MPI thread

In v0.2.0, `crates/azcp-cluster/src/stages/broadcast.rs` called
`write_all` synchronously on the main MPI thread immediately after each
`MPI_Waitany` completion. While that write was running, no new
`MPI_Ibcast` could be posted — receive and write were serialized within
each rank. On tmpfs the write completed in microseconds and the
serialization barely showed; on NVMe (where a single buffered write at
4 MiB takes hundreds of microseconds) it cost roughly half the achievable
throughput.

### The fix — async writer thread (shipped in v0.2.1)

v0.2.1 moves writes onto a dedicated writer thread per `run()`,
communicating via two `std::sync::mpsc` channels (commands main→writer,
acks writer→main). The main MPI loop hands completed buffers off and
immediately posts the next `MPI_Ibcast`; the writer drains writes in the
background. Two correctness details:

- **Positional writes (`pwrite`).** `MPI_Waitany` is not guaranteed to
  return completions in posting order, so each in-flight chunk carries an
  explicit byte offset and the writer uses `FileExt::write_all_at` rather
  than sequential `write_all`. Correctness is independent of completion
  ordering.
- **Buffer ownership / backpressure.** The MPI buffer for chunk K can't
  be reused until the write of chunk K hits disk. The buffer is
  `mem::take`-d out of the slot when handed to the writer; the slot only
  returns to the free pool when the writer's ack restores the real
  buffer. If the writer falls behind, the main loop blocks on
  `ack_rx.recv()` instead of posting more `MPI_Ibcast`s with no place to
  land.

Result: the v0.2.1 NVMe sweep at the top of this document. NVMe now
exceeds the old tmpfs ceiling (~70-76 Gb/s vs ~58 Gb/s); the disk array
was never the limit.

### Aggregate Azure download

For comparison, the `[download]` stage on the same 16-node setup hit
**228-295 Gb/s aggregate** (sum across ranks) across runs against a
single storage account, with the source fully read in 11-15 seconds.
Per-rank throughput averaged ~14-18 Gb/s. This is unchanged across
versions — only the bcast path was affected.

### What this means for tool choice

- On this hardware and v0.2.1, `azcp-cluster` `[bcast]` (~76 Gb/s per
  receiver) crushes per-node Azure throughput (~14-18 Gb/s) by ~5×
  *and* pays account egress only once instead of N times. Clear win on
  both wall-clock and egress cost for ≥10 nodes.
- The current ceiling is single-rail UCX `MPI_Bcast` tree (~70-76 Gb/s).
  Multi-rail UCX (the GB300 nodes have 4 HCAs) would push past this;
  tracked for future work.
- On a TCP-only fabric, `[bcast]` BW is ~8 Gb/s, *below* per-node
  Azure throughput on this account. At that point the egress savings
  are real but per-node `azcp --shard` may finish faster wall-clock.
  Use `azcp-cluster` for the egress savings; don't expect a
  wall-clock win.
- The shipped defaults (`--bcast-chunk 64M --bcast-pipeline 4`) work
  but leave throughput on the table on NDR-class fabrics. For RDMA
  IB, `--bcast-chunk 512M --bcast-pipeline 16` is the right setting
  on this hardware.

## Reading the results

A few caveats:

- **Single-run measurements are not statistical rigor.** Even on a quiet
  cluster, runs can swing 5-10% based on collective scheduling and
  fabric contention. Use these as ballpark numbers, not contracts. Re-run
  with `REPS=3` for a stabler median.
- **Cold-cache vs warm-cache.** All numbers above are cold (`--compare
  none`, every byte transferred). With `--compare size` and a
  pre-populated NVMe, `[total]` drops to seconds and the BW lines become
  meaningless.
- **`[bcast]` BW is per-receiver.** Multiply by `(N-1)` to get aggregate
  fabric traffic. The number listed is what each downstream rank
  observed.

## Future work

- **Multi-rail UCX bcast.** Single-rail is the current ceiling at ~76
  Gb/s per receiver; the GB300 nodes have 4 HCAs that could in principle
  push that higher.
- **Bcast tuning autodetect.** Probe the fabric at startup and pick
  chunk/pipeline accordingly, instead of shipping conservative defaults.
- **Multi-account source.** At very high rank counts the single-account
  egress ceiling becomes a real bottleneck.
- **Re-run on AMD MI300X clusters** — different fabric, different tuning.
- **Compare `azcp-cluster --compare filelist` against `rsync` over the
  fabric** for the "incremental update" use case.
