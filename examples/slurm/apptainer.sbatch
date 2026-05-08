#!/usr/bin/env bash
#SBATCH --job-name=azcp-cluster-apptainer
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1       # azcp-cluster requires exactly one rank per node
#SBATCH --cpus-per-task=8
#SBATCH --time=01:00:00
#SBATCH --output=azcp-cluster-%j.out
#
# azcp-cluster on Slurm via Apptainer (ALTERNATIVE pattern).
#
# Use this when pyxis is not available, or when site policy requires
# SIF images. For the recommended enroot path, see azcp-cluster.sbatch.
#
# One-time image build (run on a login node):
#   apptainer build /shared/images/azcp-cluster.sif \
#     docker://ghcr.io/edwardsp/azcp/azcp-cluster:v0.3.0
#
# Edit before submitting:
#   - --nodes
#   - SOURCE_URL
#   - DEST
#   - IMAGE: path to the SIF file built above.
set -euo pipefail

IMAGE="${IMAGE:-/shared/images/azcp-cluster.sif}"
SOURCE_URL="${SOURCE_URL:-https://REPLACE_WITH_ACCOUNT.blob.core.windows.net/REPLACE_WITH_CONTAINER/REPLACE_WITH_PREFIX/}"
DEST="${DEST:-/mnt/nvme/dataset}"

srun --ntasks-per-node=1 mkdir -p "$DEST"

# Apptainer launches one container per task; PMIx propagates rank info into
# each container. We bind /dev/infiniband for RDMA and /mnt/nvme for the
# destination.
srun \
  --mpi=pmix \
  --export=ALL,SOURCE_URL,DEST,IMAGE \
  bash -c '
    set -euo pipefail
    exec apptainer exec \
      --bind /dev/infiniband:/dev/infiniband \
      --bind /mnt/nvme:/mnt/nvme \
      --env UCX_TLS=rc,sm,self \
      --env UCX_NET_DEVICES=mlx5_0:1,mlx5_1:1,mlx5_2:1,mlx5_3:1 \
      --env UCX_IB_GID_INDEX=3 \
      "$IMAGE" \
      azcp-cluster \
        "$SOURCE_URL" "$DEST" \
        --concurrency 32 --block-size 16777216 \
        --bcast-chunk 268435456 --bcast-pipeline 8 \
        --compare size \
        --no-progress
  '

echo
echo "Done. Verify with:"
echo "  srun --ntasks-per-node=1 du -sh $DEST"
