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
| `--bcast-pipeline N` | 4 | Number of bcast chunks in flight per file. Higher = more pipelining, more memory. |
| `--max-retries N` | 5 | Per-HTTP-request retry budget. |
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

Defaults are reasonable for 8-32 nodes on a 100-200 GbE fabric. Two knobs
matter for bcast performance:

- `--bcast-chunk` — too small and you pay MPI per-message overhead per
  chunk; too large and you serialize the bcast (no overlap with the next
  chunk). 64 MiB is the default. On RDMA fabrics with deep queues we've
  measured improvements at 256 MiB; on TCP, smaller chunks (16 MiB) are
  often better.
- `--bcast-pipeline` — number of chunks per file in flight. 4 is the
  default. With RDMA, 8 has measurably better pipelining; on TCP 2 is
  usually the right answer (more in-flight chunks just causes head-of-line
  blocking on the single TCP connection per pair).

For first-run tuning, start with the defaults and the `[bcast] BW` line
is your scoreboard. See [cluster-benchmarks.md](cluster-benchmarks.md) for
empirical sweep results.

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
- [performance-tuning.md](performance-tuning.md) — single-node tuning
  background. Most of it doesn't apply to `azcp-cluster` (which is
  fabric-bound, not host-network-stack-bound), but the NUMA section is
  useful when sizing pod CPU requests.
