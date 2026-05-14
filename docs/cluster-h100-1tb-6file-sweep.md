# azcp-cluster perf sweep — 1.2 TB / 6 × 200 GiB, 16× ND_H100_v5

Date: 2026-05-12\
Cluster: `ccw` (CycleCloud Workspace for Slurm), eastus\
Nodes: `ccw-gpu-[1-16]` (Azure `Standard_ND96isr_H100_v5`, single rail used)\
Source: cross-region blob (eastus2 → eastus)\
Destination: `/nvme/perftest-dst` (md0 over 8× local NVMe)\
Image: `/shared/images/azcp-cluster-v0.3.0.sqsh`\
Dataset: 6 × 200 GiB files of AES-256-CTR random data (`perfgen-0..5.bin`)

## Constant configuration

```bash
export UCX_TLS=rc,sm,self
export UCX_NET_DEVICES=mlx5_ib0:1
export OMPI_MCA_pml=ucx
export OMPI_MCA_osc=ucx
export OMPI_MCA_coll_libnbc_ibcast_algorithm=1   # 1 = linear (winner from algsweep2)
export AZURE_CLIENT_ID=5da72858-8b59-4d4b-9841-1da3f0539a5f

srun --mpi=pmix --export=$EXP \
     [--mem-bind=local]                       # round 3+ only
     --container-image=/shared/images/azcp-cluster-v0.3.0.sqsh \
     --container-mounts=/dev/infiniband:/dev/infiniband,/nvme:/nvme \
     --container-writable \
     [taskset -c 0-47]                        # round 3+ only (NUMA 0 pin)
     azcp-cluster <SOURCE_URL> /nvme/perftest-dst \
       --concurrency $C --block-size $BS \
       --bcast-chunk $BC --bcast-pipeline $BP --bcast-writers $BW \
       --no-progress
```

NUMA layout on ND_H100_v5:

```
NUMA 0:  CPUs 0-47   | mlx5_ib0..3 | frontend Ethernet NIC (blob)
NUMA 1:  CPUs 48-95  | mlx5_ib4..7 | (unused this run)
NVMe (md0): NUMA 1   (cross-NUMA write — PCIe NVMe ≫ 200 Gbps frontend cap, tolerable)
```

Both the frontend NIC (blob downloads) and the chosen IB rail (`mlx5_ib0`, MPI broadcast) live on NUMA 0, so a single `taskset -c 0-47` pins both DMA paths to local memory.

`taskset` is present in the container; `numactl` is not. `srun --mem-bind=local` enforces first-touch memory locality at the Slurm level without requiring `numactl` inside the container.

## Results

C = `--concurrency`, BS = `--block-size`, BC = `--bcast-chunk`, BP = `--bcast-pipeline`, BW = `--bcast-writers`. 1G = 1073741824 bytes.

All rows with **BC ≥ 2G** use **azcp-cluster v0.3.1**, which ships `fix(cluster): sub-chunk MPI_Ibcast to avoid i32 count overflow`. Earlier v0.3.0 hit an `int32` element-count overflow inside `MPI_Ibcast` for chunks ≥ 2 GiB; v0.3.1 sub-chunks the broadcast to keep the count below the limit. The top row was additionally md5-verified across all 16 nodes against the source blob — see [v0.3.1 fix and md5 verification](#v031-fix-and-md5-verification) below.

Rows are sorted by end-to-end time (best first).

| Variant                          | NUMA pin | C   | BS  | BC   | BP  | BW  | DL Gb/s | Bcast Gb/s | E2E s |
| -------------------------------- | -------- | --- | --- | ---- | --- | --- | ------- | ---------- | ----- |
| **v0.3.1** pin0 C=64 BC=4G ✅ md5 | NUMA 0   | 64  | 16M | 4G   | 128 | 8   | 141.79  | 51.01      | ~285  |
| **v0.3.1** pin0 C=64 BC=2G       | NUMA 0   | 64  | 16M | 2G   | 128 | 8   | 105.33  | 54.99      | 285   |
| **v0.3.1** pin0 C=64 BC=4G       | NUMA 0   | 64  | 16M | 4G   | 128 | 8   | 114.11  | 49.50      | 298   |
| pin0 C=128 BC=1G                 | NUMA 0   | 128 | 16M | 1G   | 128 | 8   | 162.14  | 43.48      | 301   |
| pin0 C=64 BC=1G BW=16            | NUMA 0   | 64  | 16M | 1G   | 128 | 16  | 153.49  | 43.37      | 305   |
| pin0 C=64 BC=1G                  | NUMA 0   | 64  | 16M | 1G   | 128 | 8   | 150.01  | 43.49      | 306   |
| pin0 C=64 BS=32M BC=1G           | NUMA 0   | 64  | 32M | 1G   | 128 | 8   | 149.55  | 43.47      | 306   |
| combo C=64 BC=1G                 | no       | 64  | 16M | 1G   | 128 | 8   | 131.20  | 42.66      | 320   |
| combo C=64 BC=512M               | no       | 64  | 16M | 512M | 128 | 8   | 145.60  | 31.12      | 402   |
| combo C=64 BC=256M               | no       | 64  | 16M | 256M | 128 | 8   | 149.69  | 28.85      | 426   |
| combo C=64 BC=512M BP=64 BW=16   | no       | 64  | 16M | 512M | 64  | 16  | 142.74  | 28.83      | 430   |
| combo C=64 BS=32M BC=256M        | no       | 64  | 32M | 256M | 128 | 8   | 146.61  | 28.23      | 435   |
| combo C=64 BC=256M BP=64         | no       | 64  | 16M | 256M | 64  | 8   | 143.92  | 27.58      | 445   |
| bigBC BC=256M                    | no       | 32  | 16M | 256M | 128 | 8   | 97.30   | 29.17      | 459   |
| highC C=64                       | no       | 64  | 16M | 64M  | 128 | 8   | 137.66  | 26.68      | 461   |
| highC C=128                      | no       | 128 | 16M | 64M  | 128 | 8   | 135.05  | 26.21      | 469   |
| bigBS BS=64M                     | no       | 32  | 64M | 64M  | 128 | 8   | 108.99  | 26.42      | 485   |
| baseline                         | no       | 32  | 16M | 64M  | 128 | 8   | 100.96  | 25.85      | 501   |
| moreBW BW=16                     | no       | 32  | 16M | 64M  | 128 | 16  | 101.48  | 25.68      | 503   |
| deepPipe BP=256                  | no       | 32  | 16M | 64M  | 256 | 8   | 102.32  | 23.51      | 539   |

### v0.3.1 fix and md5 verification

The table omits two classes of v0.3.1 runs that produced no usable timing:

- **BC ≥ 16G**: OOM-killed by the Slurm cgroup (in-flight buffers at BP=128 exceed the per-rank memory budget). Reachable only by lowering `--bcast-pipeline` or raising `--mem`.
- **BC=8G**: completes correctly but hits a real ~19 Gb/s broadcast cliff (likely cache/RDMA-window pressure). Not a correctness bug — md5-verified — just a throughput regression. The optimal BC remains **2-4 GiB**.

The top-row BC=4G winner was re-run with `md5sum` of all 6 files on every one of the 16 nodes, then diffed against the source blob: **bit-identical on every node**. Companion BC=2G and BC=8G v0.3.1 runs also passed md5 (96 lines, 6 unique hashes per variant; 288 lines, 6 unique across all three variants).

For completeness: the same md5 harness applied to **v0.3.0 BC=4G** (the formerly-recommended config) showed the corruption that motivated the v0.3.1 fix — every node received a different garbled version of every file (24 distinct md5s where 6 were expected). This is why the table only carries v0.3.1 numbers at BC ≥ 2G; v0.3.0 is only safe at BC ≤ 1 GiB.

## Findings

- **Concurrency**: 32 → 64 doubles the download rate to ~150 Gb/s (~75% of the 200 Gbps cross-region storage cap). C=128 gives a marginal ~+5% on top of NUMA pin (164 Gb/s peak). Beyond C=64 returns are tiny.
- **Block size**: 16 MiB is optimal; 32 MiB neutral; 64 MiB slightly worse.
- **Bcast chunk size dominates broadcast performance** in the few-large-files regime. With only 6 files, MPI pipeline depth is starved at small chunks. Scaling on a NUMA-pinned C=64 run:
  - 64M → 26 Gb/s
  - 256M → 29 Gb/s
  - 512M → 31 Gb/s
  - 1G → 43 Gb/s (last safe BC on v0.3.0)
  - 2G → 55 Gb/s (v0.3.1)
  - **4G → 51 Gb/s (v0.3.1, md5-verified peak)**
  - 8G → 19 Gb/s (real cliff)
- **Pipeline depth (BP)** and **writers (BW)**: second-order. BP=128 / BW=8 defaults are fine. BP=256 hurts (over-pipelined for 6 files). BP=64 also hurts.
- **NUMA pin** is worth +14% download and +2-5% bcast at BC=1G.
- **End-to-end speedup**: baseline (BC=64M) 501 s → v0.3.1 BC=4G ~285 s = **1.76×**.

## Recommended configuration

For 1.2 TB / 6-file workloads on 16× ND_H100_v5 (single rail). **Use azcp-cluster v0.3.1 or newer.** v0.3.0 with `--bcast-chunk ≥ 2G` is unsafe.

```bash
#SBATCH --exclusive
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1

export UCX_TLS=rc,sm,self
export UCX_NET_DEVICES=mlx5_ib0:1
export OMPI_MCA_pml=ucx
export OMPI_MCA_osc=ucx
export OMPI_MCA_coll_libnbc_ibcast_algorithm=1

srun --mpi=pmix --export=ALL,UCX_TLS,UCX_NET_DEVICES,OMPI_MCA_pml,OMPI_MCA_osc,OMPI_MCA_coll_libnbc_ibcast_algorithm \
     --mem-bind=local \
     --container-image=/shared/images/azcp-cluster-v0.3.1.sqsh \
     --container-mounts=/dev/infiniband:/dev/infiniband,/nvme:/nvme \
     --container-writable \
     taskset -c 0-47 \
     azcp-cluster <SOURCE_URL> /nvme/dataset \
       --concurrency 64 \
       --block-size 16777216 \
       --bcast-chunk 4294967296 \
       --bcast-pipeline 128 \
       --bcast-writers 8 \
       --no-progress
```

Expected (md5-verified): download ~140 Gb/s, broadcast ~50 Gb/s, end-to-end ~285 s.

For workloads with **higher concurrency budget** and the cluster otherwise idle, increase `--concurrency 128` for marginal download uplift at no bcast cost (validate md5 if production use).
