#!/usr/bin/env bash
#SBATCH --job-name=azcp-cluster
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1       # azcp-cluster requires exactly one rank per node
#SBATCH --cpus-per-task=8
#SBATCH --time=01:00:00
#SBATCH --output=azcp-cluster-%j.out
#
# azcp-cluster on Slurm via enroot + pyxis (PRIMARY pattern).
#
# Edit before submitting:
#   - --nodes: how many nodes to broadcast across.
#   - SOURCE_URL: the blob prefix to broadcast.
#   - DEST: per-node destination on local NVMe.
#   - IMAGE: path to the squashfs imported by enroot-import.sh.
#   - AZURE_CLIENT_ID: only set if your nodes have multiple managed
#     identities. With a single system-assigned identity, leave it
#     unset and IMDS will pick the right one.
#
# Submit:
#   sbatch examples/slurm/azcp-cluster.sbatch
#   tail -f azcp-cluster-<jobid>.out
#
# See docs/cluster-slurm.md for prerequisites and troubleshooting.
set -euo pipefail

IMAGE="${IMAGE:-/shared/images/azcp-cluster.sqsh}"
SOURCE_URL="${SOURCE_URL:-https://REPLACE_WITH_ACCOUNT.blob.core.windows.net/REPLACE_WITH_CONTAINER/REPLACE_WITH_PREFIX/}"
DEST="${DEST:-/mnt/nvme/dataset}"

# Make every node create the destination on its local NVMe before launch.
# pyxis runs the container as root by default; if your site overrides this,
# the dest path needs to be pre-created with the right perms.
srun --ntasks-per-node=1 mkdir -p "$DEST"

# srun launches one azcp-cluster rank per node. Open MPI (in the container)
# picks up ranks from PMIx without needing an explicit mpirun.
srun \
  --mpi=pmix \
  --container-image="$IMAGE" \
  --container-mounts=/dev/infiniband:/dev/infiniband,/mnt/nvme:/mnt/nvme \
  --container-writable \
  --export=ALL,SOURCE_URL,DEST \
  --no-container-entrypoint \
  bash -c '
    set -euo pipefail
    # UCX env mirrors what mpirun --pml ucx sets up on AKS. PMIx sets the
    # rest (PMIX_*, OMPI_*).
    export UCX_TLS=rc,sm,self
    export UCX_NET_DEVICES=mlx5_0:1,mlx5_1:1,mlx5_2:1,mlx5_3:1
    export UCX_IB_GID_INDEX=3
    exec azcp-cluster \
      "$SOURCE_URL" "$DEST" \
      --concurrency 32 --block-size 16777216 \
      --bcast-chunk 268435456 --bcast-pipeline 8 \
      --compare size \
      --no-progress
  '

echo
echo "Done. Verify with:"
echo "  srun --ntasks-per-node=1 du -sh $DEST"
