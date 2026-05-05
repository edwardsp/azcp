# `azcp-cluster` on Slurm

Two container runtimes are supported. **enroot + pyxis is the recommended
path** on Azure HPC clusters and on most NVIDIA-built Slurm setups (DGX,
NGC base). Apptainer (formerly Singularity) is the alternative for sites
that don't have pyxis or prefer SIF images.

Both consume the same OCI image published to GHCR — there is no
Slurm-specific build of `azcp-cluster`.

## Pattern A — enroot + pyxis (recommended)

### Prerequisites

- Slurm with the [pyxis](https://github.com/NVIDIA/pyxis) SPANK plugin
  enabled (provides `srun --container-image=...`).
- [enroot](https://github.com/NVIDIA/enroot) installed on every compute
  node (pyxis depends on it).
- A shared filesystem reachable from every node, used for:
  - The imported container image (`.sqsh` file).
  - The dataset destination (or per-node NVMe — see "Destination
    placement" below).
- Each compute node has Azure credentials available to the container.
  Recommended: Azure VM **system-assigned managed identity** with **Storage
  Blob Data Reader** on the source account, accessed via IMDS
  (`169.254.169.254`). For multi-identity hosts, set `AZURE_CLIENT_ID`.

### Importing the image

Import once on a login node (or wherever your shared filesystem is
writable):

```bash
# See examples/slurm/enroot-import.sh for a complete script.
enroot import -o /shared/images/azcp-cluster.sqsh \
  docker://ghcr.io#edwardsp/azcp/azcp-cluster:v0.2.0
```

This produces a `~250 MiB` squashed image. Re-import when you bump
versions; tag it in the filename so you can hold multiple versions side
by side.

### Submitting a job

A complete sbatch script is in
[`examples/slurm/azcp-cluster.sbatch`](../examples/slurm/azcp-cluster.sbatch).
The shape is:

```bash
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1     # one azcp-cluster rank per node
#SBATCH --gres=...              # whatever your IB resource is (often unused on dataloader nodes)

srun \
  --mpi=pmix \
  --container-image=/shared/images/azcp-cluster.sqsh \
  --container-mounts=/dev/infiniband:/dev/infiniband,/mnt/nvme:/mnt/nvme \
  --container-writable \
  --export=ALL,AZURE_CLIENT_ID,SOURCE_URL \
  azcp-cluster "$SOURCE_URL" /mnt/nvme/dataset \
    --concurrency 32 --block-size 16777216 \
    --bcast-chunk 67108864 --bcast-pipeline 4 \
    --compare size --no-progress
```

Key flags:

- `--mpi=pmix` — Slurm's PMIx integration; `azcp-cluster`'s Open MPI build
  picks ranks up from PMIx without needing an explicit `mpirun`.
- `--ntasks-per-node=1` — `azcp-cluster` aborts if it sees more than one
  rank per node.
- `--container-mounts=/dev/infiniband:/dev/infiniband` — required for
  RDMA. Without it, UCX silently falls back to TCP.
- `--container-mounts=/mnt/nvme:/mnt/nvme` — destination directory.

### Destination placement

The dataset must end up at the same path on every node. Two patterns:

1. **Per-node NVMe (recommended for cold downloads).** Each node writes
   to its own local NVMe at `/mnt/nvme/dataset`. The `[bcast]` stage
   makes sure every node has the full dataset. This is the standard
   distributed-training setup.
2. **Shared filesystem (Lustre, Azure Managed Lustre, etc.).** Mount the
   shared FS at the same path on every node and write there. The
   `[bcast]` stage is wasted in this case (every rank "broadcasts" to
   the same shared destination), so you might as well use plain `azcp`
   with `--shard $SLURM_PROCID/$SLURM_NTASKS` — `azcp-cluster` is the
   wrong tool for shared-storage destinations.

### Watching the job

```bash
sbatch examples/slurm/azcp-cluster.sbatch
squeue -u $USER
tail -f slurm-<jobid>.out
```

Rank 0's `[list]` / `[diff]` / `[download]` / `[bcast]` / `[filelist]` /
`[total]` summary appears in the slurm out file. PMIx handles output
multiplexing across ranks the same way mpirun does.

## Pattern B — Apptainer (alternative)

Use this when pyxis isn't available, when policy requires SIF images, or
when you want a simpler model that doesn't depend on the Slurm SPANK
plugin layer.

### Importing the image

```bash
apptainer build /shared/images/azcp-cluster.sif \
  docker://ghcr.io/edwardsp/azcp/azcp-cluster:v0.2.0
```

This is slower than `enroot import` (Apptainer rebuilds an mksquashfs
layer) but produces a portable single-file SIF.

### Submitting a job

A complete sbatch script is in
[`examples/slurm/apptainer.sbatch`](../examples/slurm/apptainer.sbatch).
The shape:

```bash
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1

srun --mpi=pmix \
  apptainer exec \
    --bind /dev/infiniband:/dev/infiniband \
    --bind /mnt/nvme:/mnt/nvme \
    --env AZURE_CLIENT_ID="$AZURE_CLIENT_ID" \
    /shared/images/azcp-cluster.sif \
    azcp-cluster "$SOURCE_URL" /mnt/nvme/dataset \
      --concurrency 32 --block-size 16777216 \
      --bcast-chunk 67108864 --bcast-pipeline 4 \
      --compare size --no-progress
```

The big functional difference vs pyxis: pyxis sets up the container
namespace **once per srun step** and reuses it across all tasks on a node;
Apptainer launches a fresh container per task. For `azcp-cluster` (one
task per node) the difference is negligible, but for tools that fan out
many tasks per node it matters.

## Authentication on Slurm-on-Azure

The same credential resolution order from `azcp` applies. The most common
production setup:

1. Each compute node has a system-assigned **managed identity** with
   **Storage Blob Data Reader** on the source storage account.
2. The container has access to IMDS at `169.254.169.254` (the default for
   Azure VMs; pyxis and Apptainer both pass through host networking by
   default).
3. No environment variables required — the container detects the IMDS
   endpoint and gets a Bearer token.

For multi-identity setups (more than one user-assigned identity on the
node), set `AZURE_CLIENT_ID` to the desired identity and `--export` it
through srun.

If your cluster denies IMDS access from inside containers, the fallbacks
are SAS (`AZURE_STORAGE_SAS_TOKEN`) or shared key
(`AZURE_STORAGE_ACCOUNT` + `AZURE_STORAGE_KEY`). SAS tokens expire
mid-transfer on long downloads — generate them with at least 2× the
expected runtime, or use shared key.

## Common gotchas

- **`--ntasks-per-node` must be 1.** `azcp-cluster` aborts otherwise.
  Double-check the sbatch header — Slurm's defaults can vary by partition.
- **`/dev/infiniband` mount is mandatory for RDMA.** With pyxis, add it to
  `--container-mounts`. With Apptainer, add a `--bind`. Without it, UCX
  falls back to TCP and `[bcast]` BW drops by ~10×.
- **`IPC_LOCK` / `ulimit -l unlimited`.** Most HPC schedulers set this for
  you (it's required by NCCL), but if you're running on a stock Slurm
  install, you may need a `--gres` or `--export=ULIMIT_MEMLOCK=unlimited`
  hack. The pyxis path inherits the host's `ulimit -l`.
- **`/dev/shm` size.** UCX's intra-node transport uses shared memory.
  Slurm's default shm is often only 64 MiB, which throttles any same-node
  test. For real multi-node runs this rarely matters (one rank per node →
  no intra-node UCX traffic), but watch out for single-node testing.
- **`--mpi=pmix` vs `--mpi=pmi2`.** Both work with the image's Open MPI
  build. PMIx is the modern default; PMI2 is the fallback for older
  Slurm installations. If `srun` complains about an unknown MPI plugin,
  try the other one.

## Troubleshooting

- **`Could not initialize PMIx` on srun**: your Slurm wasn't built with
  PMIx, or the PMIx version mismatches the container's Open MPI. Try
  `--mpi=pmi2` or `--mpi=none` (the latter requires you to invoke
  `mpirun` inside the container instead).
- **`401`/`403` on LIST**: the node's managed identity doesn't have
  Storage Blob Data Reader on the source account, or `AZURE_CLIENT_ID`
  points at an identity that isn't attached to the node.
- **`[bcast]` BW lower than `[download]` BW**: UCX did not pick RDMA.
  Check that `/dev/infiniband` is mounted into the container, that the
  job has `--export=...UCX_TLS=rc,sm,self`, and run `ucx_info -d` inside
  the container to confirm UCX sees the IB devices.
- **`mpirun` (or PMIx) hangs at startup**: usually a hostname-resolution
  issue. Confirm `getent hosts $(scontrol show hostname)` returns a
  routable IP from inside the container.
