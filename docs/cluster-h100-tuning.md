# Performance Tuning on H100 — MPI Broadcast Workloads on Azure ND H100 v5

A walkthrough of an end-to-end performance tuning exercise on **16 nodes of `Standard_ND96isr_H100_v5`**, optimizing an MPI-broadcast-based file distribution workload (`azcp-cluster` pulling a 413 GB model from Azure Blob Storage to local NVMe). The same techniques apply to any MPI-collective-bound workload on this SKU.

The workload starts at **75 Gb/s broadcast / 77 s wall-clock**. With three tuning changes we reach **110 Gb/s broadcast / 48 s wall-clock** — a **37% reduction in total time** with zero code changes and zero hardware reconfiguration. The biggest single lever (NUMA pinning) is one extra command in your launcher.

---

## 1. Target environment

| Item | Value |
|---|---|
| **VM SKU** | `Standard_ND96isr_H100_v5` |
| **Per-node hardware** | 8× H100 80 GB, 8× ConnectX-7 NDR (400 Gb/s each), 96 vCPU split across 2 NUMA nodes, 1.8 TB RAM |
| **NUMA layout** | NUMA 0 = CPUs 0-47, IB devices `mlx5_ib0..3`, **frontend (Azure) NIC**. NUMA 1 = CPUs 48-95, IB devices `mlx5_ib4..7` |
| **Cluster size** | 16 nodes |
| **Scheduler** | Slurm (Azure CycleCloud Workspace for Slurm), pyxis + enroot |
| **Container image** | `ghcr.io/edwardsp/azcp/azcp-cluster:v0.3.0` (Open MPI 4.1.6 + UCX 1.15) |
| **Region / fabric** | `eastus`, NDR InfiniBand |

The cluster used for these measurements was provisioned with
[**ai-infrastructure-on-azure**](https://github.com/Azure/ai-infrastructure-on-azure) —
the Azure reference deployment for AI/HPC clusters (CycleCloud Workspace
for Slurm, pyxis/enroot, NDR InfiniBand wiring, NCCL/HPCX baked in). If
you are reproducing this guide on a different stack, check that the
NUMA-to-NIC mapping and the IB device names match what's in
[§5.5](#55-numa-pinning--the-biggest-single-win) before copying the
pinning recipe verbatim.

The workload uses **`MPI_Ibcast`** (non-blocking broadcast) via Open MPI inside a container. One MPI rank per node. This guide is most directly applicable to anything that issues collectives over the same Open MPI / UCX stack on this SKU.

---

## 2. Workload

`azcp-cluster` distributes a `nvidia/DeepSeek-R1-0528-NVFP4-v2` model checkpoint:

- 524 files, **413,340,567,567 bytes ≈ 413 GB**
- Stage 1: rank 0 downloads from Azure Blob (Private Endpoint, Azure backbone, eastus → eastus2)
- Stage 2: rank 0 broadcasts to all other ranks via pipelined `MPI_Ibcast` (64 MiB chunks, depth 128)
- Stage 3: each rank writes its received chunks to local NVMe (md0 RAID-0 over 8× NVMe, XFS, O_DIRECT)

Best invocation flags (independently tuned on this image):

```
azcp-cluster <src> <dst> \
  --bcast-chunk 67108864 --bcast-pipeline 128 --bcast-writers 8 \
  --concurrency 32 --block-size 16777216 --no-progress
```

---

## 3. Methodology

Every measurement is a full end-to-end run with `/nvme/dataset` cleared at start. We report `[download]`, `[bcast]`, `[total]` from the application's own timing. Repeated as 3-iter validation for shortlisted configurations.

For shape verification of "is the network healthy?", we ran NCCL `all_reduce_perf` (HPCX 2.25.1) at 16 nodes × 8 GPUs (128 ranks):

| Message size | OOP busbw |
|---|---|
| 4 GiB | **446 GB/s** (peak; matches H100 v5 baseline ~450 GB/s) |
| 64 MiB (= our bcast chunk size) | ~390 GB/s |
| 16 GiB | 390-450 GB/s (high variance) |

NCCL hits ~3.5 Tb/s aggregate — the InfiniBand fabric and the GPUs/NICs are healthy. **The bottleneck is in the MPI / I/O stack, not the network.** This is the cue to tune software, not hardware.

---

## 4. Baseline

Default config — no MPI tuning, single rail, no pinning:

| Metric | Value |
|---|---|
| download | ~100 Gb/s |
| bcast | ~75 Gb/s |
| total | ~77 s |

Required environment for any UCX-based MPI run on this SKU:

```bash
export UCX_TLS=rc,sm,self
export OMPI_MCA_pml=ucx
export OMPI_MCA_osc=ucx
export UCX_NET_DEVICES=mlx5_ib0:1   # single-rail: pin to one IB device
```

---

## 5. The diagnostic journey

### 5.1 First instinct: try `OMPI_MCA_coll_tuned_bcast_algorithm` — produced no signal

Open MPI's `coll: tuned` component exposes an algorithm selector for `MPI_Bcast`:

```bash
export OMPI_MCA_coll_tuned_use_dynamic_rules=1
export OMPI_MCA_coll_tuned_bcast_algorithm=$ALG    # 1..9
```

Sweep on v0.3.0 (single iter each):

| Algo | Name | bcast Gb/s | Δ vs baseline |
|---|---|---:|---:|
| (default) | tuned default | 75.05 | — |
| 1 | basic_linear | 74.61 | -0.4 |
| 3 | pipeline | 73.38 | -1.7 |
| 5 | binary_tree | 74.06 | -1.0 |
| 6 | binomial_tree | 69.12 | -5.9 |
| 7 | knomial_tree | 72.12 | -2.9 |
| 8 | scatter_allgather | 72.09 | -3.0 |
| 9 | scatter_allgather_ring | 71.85 | -3.2 |

All within run-to-run noise. **The supposedly winning `scatter_allgather` was actually slightly worse than the default.** This was the diagnostic clue.

### 5.2 The trap: `tuned` controls *blocking* `MPI_Bcast`. Our workload uses `MPI_Ibcast`.

The application calls `MPI_Ibcast` (the non-blocking variant). Open MPI dispatches non-blocking collectives through the **`coll: libnbc`** component (priority 10) by default, **not** `coll: tuned`. The `OMPI_MCA_coll_tuned_*` knobs we set were inert for our workload.

You can confirm what your binary is calling with `nm -D /path/to/libmpi.so | grep -i ibcast`, or `strace`, or with the source.

The right knob for non-blocking broadcast is:

```bash
export OMPI_MCA_coll_libnbc_ibcast_algorithm=$ALG   # 1=linear, 2=binomial, 3=chain, 4=knomial
```

Inspect available knobs with:

```bash
ompi_info --param coll libnbc --level 9 | grep -iE 'ibcast|bcast'
ompi_info --param coll all --level 9 | grep -iE 'priority'
```

### 5.3 Corrected sweep — `libnbc` (the right knob)

Single iter each on v0.3.0:

| Variant | bcast Gb/s | total s | Δ vs baseline |
|---|---:|---:|---:|
| baseline (default) | 68.12 | 79.48 | — |
| **`libnbc_ibcast_algorithm=1` (linear)** | **101.19** | **65.27** | **+48% / -18%** |
| `libnbc_ibcast_algorithm=4` (knomial) | 82.13 | 71.70 | +21% / -10% |
| `libnbc_ibcast_algorithm=2` (binomial) | 74.25 | 74.35 | +9% / -6% |
| `libnbc_ibcast_algorithm=3` (chain) | OOM | OOM | crashed (memory pressure at depth 128) |
| `coll_adapt_priority=100` (enable adapt) | 75.84 | 74.95 | +11% / -6% |
| `coll_han_priority=100` (enable han) | 71.31 | 77.76 | +5% / -2% |

**Linear wins decisively.** Why? With one rank per node and a 400 Gb/s NIC at the root, the bottleneck is the root's outbound bandwidth. `MPI_Ibcast` is non-blocking and pipelined to depth 128, so even though linear is "send to each receiver one at a time," the receivers' work overlaps with subsequent root sends. Tree algorithms add intermediate-node coordination overhead without a corresponding bandwidth benefit at this scale.

**Why chain OOM'd:** chain holds intermediate buffers along the chain-of-ranks before forwarding; with 128 in-flight broadcasts × 64 MiB × per-link buffering it exceeded available memory. Reduce `--bcast-pipeline` to use chain, or just use linear.

### 5.4 Validation: 3 iters of `libnbc_ibcast_algorithm=1`

| Iter | download | bcast | total |
|---|---:|---:|---:|
| 1 | 98.32 | 102.15 | 66.19 |
| 2 | 107.74 | 97.47 | 64.80 |
| 3 | 99.52 | 85.18 | 72.53 |
| **mean** | **101.86** | **94.93** | **67.84** |
| σ | 4.0 | 7.4 | 3.3 |

Stable. Confirms ~25% bcast win is reproducible.

### 5.5 NUMA pinning — the biggest single win

ND H100 v5 has 2 NUMA nodes. NICs split 4-on-NUMA-0, 4-on-NUMA-1:

```
$ for n in /sys/devices/system/node/node[0-9]*; do echo "$n cpus: $(cat $n/cpulist)"; done
node0 cpus: 0-47
node1 cpus: 48-95

$ for d in /sys/class/infiniband/mlx5_ib*; do echo "$d -> numa $(cat $d/device/numa_node)"; done
mlx5_ib0 -> numa 0
mlx5_ib1 -> numa 0
mlx5_ib2 -> numa 0
mlx5_ib3 -> numa 0
mlx5_ib4 -> numa 1
mlx5_ib5 -> numa 1
mlx5_ib6 -> numa 1
mlx5_ib7 -> numa 1
```

Without explicit pinning, Slurm/Linux can schedule the rank's threads on the wrong NUMA node relative to the chosen NIC, causing every send/write to cross UPI. Bind the process to the same NUMA node as the NIC:

```bash
# Submit job with --exclusive to get all 96 cores in the cgroup
#SBATCH --exclusive

# Inside the container (or on the host), pin to NUMA-0 cores when using mlx5_ib0
srun ... taskset -c 0-47 azcp-cluster ...
```

3-iter result with `libnbc_ibcast_algorithm=1` + `taskset -c 0-47`:

| Iter | download | bcast | total |
|---|---:|---:|---:|
| 1 | 187.94 | 110.19 | 47.89 |
| 2 | 186.28 | 120.10 | 45.47 |
| 3 | 175.70 | 100.59 | 51.86 |
| **mean** | **183.31** | **110.29** | **48.41** |
| σ | 6.7 | 9.8 | 3.3 |

**Both stages benefit.** The download nearly doubled (102 → 183 Gb/s), and the broadcast climbed another ~15% (95 → 110 Gb/s). The reason both stages benefit from the same `taskset -c 0-47` is that on ND H100 v5 the **frontend (Azure) NIC is also on NUMA 0**, alongside `mlx5_ib0..3`. Pinning the rank to NUMA-0 cores therefore co-locates it with both the frontend NIC (used by the Blob download stage) *and* the IB rail (used by the bcast stage). The Blob HTTP path, the NVMe write path, and the IB sends all run against NUMA-0-local memory and skip the cross-socket UPI hop.

If you are using a non-NUMA-0 device (e.g. `mlx5_ib4` for a second-rank-per-node design), pin to `48-95` instead.

---

## 6. Combined result

| Configuration | download | bcast | total |
|---|---:|---:|---:|
| Default (untuned, unpinned) | 100 | 75 | 77 s |
| + `libnbc_ibcast_algorithm=1` | 102 | 95 | 68 s |
| + `taskset -c 0-47` | **183** | **110** | **48 s** |
| **Improvement vs default** | **+83%** | **+47%** | **-37%** |

---

## 7. Recommended configuration (copy-paste)

```bash
#!/bin/bash
#SBATCH --partition=gpu
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1
#SBATCH --exclusive            # critical: needed for taskset to access NUMA-0 cores

# Required UCX/OMPI for ND H100 v5
export UCX_TLS=rc,sm,self
export UCX_NET_DEVICES=mlx5_ib0:1
export OMPI_MCA_pml=ucx
export OMPI_MCA_osc=ucx

# The win: select linear algorithm for non-blocking broadcast
export OMPI_MCA_coll_libnbc_ibcast_algorithm=1

EXP='ALL,UCX_TLS,UCX_NET_DEVICES,OMPI_MCA_pml,OMPI_MCA_osc,OMPI_MCA_coll_libnbc_ibcast_algorithm'

srun --mpi=pmix --export=$EXP \
     --container-image=/path/to/azcp-cluster.sqsh \
     --container-mounts=/dev/infiniband:/dev/infiniband,/nvme:/nvme \
     --container-writable \
     taskset -c 0-47 \
     azcp-cluster <SOURCE_URL> /nvme/dataset \
       --bcast-chunk 67108864 --bcast-pipeline 128 --bcast-writers 8 \
       --concurrency 32 --block-size 16777216 --no-progress
```

---

## 8. Applying this to your own workload

1. **Verify the network first.** Run NCCL `all_reduce_perf` at your scale and message size to confirm the fabric is healthy. If NCCL is at the published H100 v5 baseline (~450 GB/s busbw at 16 nodes), your bottleneck is in software.
2. **Identify the actual MPI primitive your application calls.** `MPI_Bcast` and `MPI_Ibcast` are tuned by *different* coll components (`tuned` vs `libnbc`). Don't assume — check with `nm`/`strace`/source.
3. **List available coll components and their priorities** with `ompi_info --param coll all --level 9 | grep priority` so you know which one is actually serving your collective.
4. **Sweep the *right* knob** with single iters first; only run multi-iter validation on the top 1-2 configurations.
5. **Always pin to NUMA.** On ND H100 v5 specifically: `taskset -c 0-47` for `mlx5_ib0..3`, `taskset -c 48-95` for `mlx5_ib4..7`. This requires `#SBATCH --exclusive` so the cgroup actually exposes those cores.
6. **Don't blindly stack optimizations.** Optimisations that target a non-bottleneck stage can be neutral or even hurt — they trade complexity for nothing. Re-measure after each change.

---

## 9. What we did not pursue (and why)

| Knob | Status | Reason |
|---|---|---|
| Mellanox `hcoll` (SHARP offload) | Not available | Not built into the `azcp-cluster:v0.3.0` container. Would unlock another tier of bcast performance via in-network reduction; consider rebuilding the container against HPCX. |
| `OMPI_MCA_coll_libnbc_ibcast_segmentsize` | Doesn't exist | `libnbc` ibcast does not segment internally. Application-level pipelining (`--bcast-pipeline`) is the relevant knob. |
| `coll: adapt` with custom algorithm | Tested, marginal | Worth deeper exploration of `coll_adapt_bcast_algorithm` and `coll_adapt_bcast_segment_size` if linear hits a ceiling. |
| `coll: han` (hierarchical) | Tested, no gain | At 1 rank/node there is no intra-node level for han to exploit. Useful for multi-rank-per-node setups. |
| GPU-direct RDMA path | N/A | Workload is host-memory; data lands on NVMe, not GPU memory. |

---

## 10. Reproducibility

All measurements above were collected on a 16-node `Standard_ND96isr_H100_v5` cluster in `eastus`, provisioned via [ai-infrastructure-on-azure](https://github.com/Azure/ai-infrastructure-on-azure), with the model in a Premium Block Blob storage account in `eastus2` accessed via a Private Endpoint over the Azure backbone. Each datapoint is a full end-to-end run with `/nvme/<dest>` cleared between runs. Run-to-run variance (σ) is reported alongside multi-iter means.
