#!/usr/bin/env bash
#SBATCH --job-name=download-then-train
#SBATCH --nodes=4
#SBATCH --ntasks-per-node=1
#SBATCH --gres=gpu:8
#SBATCH --cpus-per-task=64
#SBATCH --time=04:00:00
#SBATCH --output=download-then-train-%j.out
#
# Two-stage Slurm pattern: azcp-cluster downloads + broadcasts a model
# checkpoint to per-node /mnt/nvme via MPI/UCX, then a downstream training
# (or inference, fine-tune, eval, ...) workload runs on the same nodes
# and reads from the local NVMe.
#
# Why this is short on Slurm and long on AKS:
#   The two stages run as two srun steps inside ONE sbatch allocation.
#   Slurm holds the node reservation across both, so per-node /mnt/nvme
#   state written by stage 1 is naturally available to stage 2 — no
#   sentinel files, no priority classes, no operator-coordination needed.
#
# Edit before submitting:
#   - --nodes / --gres: cluster shape and GPU layout
#   - AZCP_IMAGE / TRAINING_IMAGE: enroot squashfs paths
#   - SOURCE_URL: blob prefix to broadcast
#   - DEST: per-node destination on local NVMe
#   - The "stage 2" srun command at the bottom: replace with your real
#     training/inference launcher (torchrun, vllm, deepspeed, ...)
#
# Submit:
#   sbatch examples/slurm/download-then-train.sbatch
#
# Prerequisites:
#   - enroot + pyxis on the cluster (standard on Azure CycleCloud Slurm).
#   - azcp-cluster squashfs imported via examples/slurm/enroot-import.sh.
#   - Training image imported similarly:
#       enroot import -o /shared/images/training.sqsh \
#         docker://nvcr.io/nvidia/pytorch:24.01-py3
set -euo pipefail

AZCP_IMAGE="${AZCP_IMAGE:-/shared/images/azcp-cluster.sqsh}"
TRAINING_IMAGE="${TRAINING_IMAGE:-/shared/images/training.sqsh}"
SOURCE_URL="${SOURCE_URL:-https://REPLACE_WITH_ACCOUNT.blob.core.windows.net/REPLACE_WITH_CONTAINER/REPLACE_WITH_PREFIX/}"
DEST="${DEST:-/mnt/nvme/dataset}"

srun --ntasks-per-node=1 mkdir -p "$DEST"

# ─── Stage 1: download + bcast ──────────────────────────────────────────
# One azcp-cluster rank per node. PMIx wires up MPI; Open MPI inside the
# container picks up the world from there.
echo "=== Stage 1: azcp-cluster download + broadcast ==="
time srun \
  --mpi=pmix \
  --container-image="$AZCP_IMAGE" \
  --container-mounts=/dev/infiniband:/dev/infiniband,/mnt/nvme:/mnt/nvme \
  --container-writable \
  --export=ALL,SOURCE_URL,DEST \
  --no-container-entrypoint \
  bash -c '
    set -euo pipefail
    export UCX_TLS=rc,sm,self
    export UCX_NET_DEVICES=mlx5_0:1,mlx5_1:1,mlx5_2:1,mlx5_3:1
    export UCX_IB_GID_INDEX=3
    exec azcp-cluster \
      "$SOURCE_URL" "$DEST" \
      --concurrency 32 --block-size 16777216 \
      --bcast-chunk 536870912 --bcast-pipeline 16 \
      --compare size \
      --no-progress
  '

echo "=== Stage 1 complete. Per-node disk usage: ==="
srun --ntasks-per-node=1 du -sh "$DEST"

# ─── Stage 2: training / inference / your workload ─────────────────────
# Reads the model from $DEST on local NVMe. Different image, different
# entrypoint, same allocation — nodes don't move.
#
# This example shows torchrun launching a multi-node distributed training
# job. Swap for whatever you actually want to run:
#   - vLLM serving:    vllm serve "$DEST" --tensor-parallel-size 8
#   - DeepSpeed:       deepspeed --hostfile=$SLURM_HOSTFILE train.py ...
#   - Single-node:     just drop --nnodes / --node_rank
#   - Another MPI app: srun --mpi=pmix ... mpi_thing
echo "=== Stage 2: training ==="
MASTER_ADDR=$(scontrol show hostnames "$SLURM_JOB_NODELIST" | head -n1)
MASTER_PORT=29500

time srun \
  --container-image="$TRAINING_IMAGE" \
  --container-mounts=/mnt/nvme:/mnt/nvme,$HOME:$HOME \
  --container-workdir=$HOME \
  --export=ALL,DEST,MASTER_ADDR,MASTER_PORT \
  --no-container-entrypoint \
  bash -c '
    set -euo pipefail
    torchrun \
      --nnodes='"$SLURM_NNODES"' \
      --nproc-per-node=8 \
      --node-rank=$SLURM_NODEID \
      --master-addr=$MASTER_ADDR \
      --master-port=$MASTER_PORT \
      train.py --model-path "$DEST" "$@"
  '
