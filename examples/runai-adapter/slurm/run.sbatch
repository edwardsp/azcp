#!/usr/bin/env bash
#SBATCH --job-name=azcp-runai
#SBATCH --nodes=16
#SBATCH --ntasks-per-node=1
#SBATCH --gres=gpu:8
#SBATCH --cpus-per-task=32
#SBATCH --time=00:30:00
#SBATCH --output=azcp-runai-%j.out
#
# 16-node azcp + RunAI Model Streamer load benchmark.
#
# One Slurm task per node; each task runs torchrun --nproc-per-node=1 so we
# get one PyTorch rank per node (world_size=NNODES). NCCL/IB does inter-node
# broadcast. The adapter handles azcp download partitioning per rank.
#
# Edit before submitting (or pass via env):
#   IMAGE       — enroot squashfs of azcp-runai-adapter image
#   MODEL_URI   — Azure blob prefix (must end in /)
#   SHARDLIST   — pre-generated TSV manifest, on a shared FS
#   LOCAL_CACHE — per-node NVMe path
#
# Generate SHARDLIST once before submitting (any host with azcp + creds):
#   azcp ls "$MODEL_URI" --recursive --machine-readable > "$SHARDLIST"
#
# Submit:
#   sbatch examples/runai-adapter/slurm/run.sbatch

set -euo pipefail

IMAGE="${IMAGE:-/shared/images/azcp-runai-adapter.sqsh}"
MODEL_URI="${MODEL_URI:-https://REPLACE.blob.core.windows.net/models/llama3-70b/}"
SHARDLIST="${SHARDLIST:-/shared/manifests/llama3-70b.shardlist}"
LOCAL_CACHE="${LOCAL_CACHE:-/mnt/nvme/cache}"
WORKERS="${WORKERS:-4}"
CONCURRENCY="${CONCURRENCY:-32}"

if [[ ! -f "$SHARDLIST" ]]; then
  echo "ERROR: $SHARDLIST not found." >&2
  echo "Generate with: azcp ls '$MODEL_URI' --recursive --machine-readable > '$SHARDLIST'" >&2
  exit 1
fi

srun --ntasks-per-node=1 mkdir -p "$LOCAL_CACHE"

# torchrun rendezvous: head node of the allocation hosts the c10d store.
HEAD_NODE=$(scontrol show hostnames "$SLURM_JOB_NODELIST" | head -n1)
HEAD_IP=$(getent hosts "$HEAD_NODE" | awk '{print $1; exit}')
RDZV_ENDPOINT="${HEAD_IP}:29500"
NNODES=${SLURM_JOB_NUM_NODES}

echo "[launcher] nodes=$NNODES head=$HEAD_NODE($HEAD_IP) image=$IMAGE"
echo "[launcher] model_uri=$MODEL_URI"
echo "[launcher] shardlist=$SHARDLIST"

srun \
  --container-image="$IMAGE" \
  --container-mounts=/dev/infiniband:/dev/infiniband,/mnt/nvme:/mnt/nvme,/shared:/shared \
  --container-writable \
  --no-container-entrypoint \
  --export=ALL,MODEL_URI,SHARDLIST,LOCAL_CACHE,WORKERS,CONCURRENCY,NNODES,RDZV_ENDPOINT \
  bash -c '
    set -euo pipefail

    # NCCL transport: enable IB, point at the 8x CX-7 HCAs on ND H100 v5
    # (4x on GB300 — extra entries are silently ignored). GID index 3 is
    # RoCEv2 / IB on Azure SKUs.
    export NCCL_DEBUG=WARN
    export NCCL_SOCKET_IFNAME=eth0
    export NCCL_IB_HCA=mlx5_ib0,mlx5_ib1,mlx5_ib2,mlx5_ib3,mlx5_ib4,mlx5_ib5,mlx5_ib6,mlx5_ib7
    export NCCL_IB_GID_INDEX=3

    # Multi-rail tuning: spread NCCL traffic across all listed HCAs.
    # QPS_PER_CONNECTION=4 and SPLIT_DATA_ON_QPS=1 let NCCL stripe a single
    # bcast across multiple QPs (and thus multiple rails). MIN_NCHANNELS
    # ensures enough parallel SM channels to saturate the QPs. These four
    # together are the priority-1 win identified in the design doc.
    export NCCL_IB_QPS_PER_CONNECTION=${NCCL_IB_QPS_PER_CONNECTION:-4}
    export NCCL_IB_SPLIT_DATA_ON_QPS=${NCCL_IB_SPLIT_DATA_ON_QPS:-1}
    export NCCL_MIN_NCHANNELS=${NCCL_MIN_NCHANNELS:-32}
    export NCCL_NET_GDR_LEVEL=${NCCL_NET_GDR_LEVEL:-PHB}

    NODE_RANK=${SLURM_NODEID}

    exec torchrun \
      --nnodes=${NNODES} \
      --nproc-per-node=1 \
      --node-rank=${NODE_RANK} \
      --rdzv-backend=c10d \
      --rdzv-endpoint=${RDZV_ENDPOINT} \
      /opt/runai-adapter/test_load.py \
        --model-uri "${MODEL_URI}" \
        --shardlist "${SHARDLIST}" \
        --local-cache "${LOCAL_CACHE}" \
        --workers ${WORKERS} \
        --concurrency ${CONCURRENCY} \
        --validate
  '

echo
echo "Done."
