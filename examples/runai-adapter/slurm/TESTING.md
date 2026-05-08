# Running the azcp + RunAI adapter on Slurm (16 nodes)

End-to-end procedure to validate the adapter against a real model on a
Slurm cluster with enroot + pyxis (the standard Azure CycleCloud and
NVIDIA Base Command setup).

## 0. Prerequisites

On the cluster:
- Slurm with **pyxis** + **enroot** (`srun --container-image=...` works).
- Per-node NVMe at a known path (typical: `/mnt/nvme` or `/mnt/resource/nvme`).
- Shared filesystem reachable from every node (NFS / Lustre / AzureFile)
  for the shardlist and the enroot squashfs.
- IB / RoCE working between nodes (test with `ucx_perftest` or `ib_send_bw`).
- ND H100 v5 (8× CX-7) or GB300 (4× CX-7) node SKU.

On your laptop (or any machine with the `gh` CLI):
- `docker` or `podman` to pull the image (optional — enroot can pull directly).
- Azure credentials with read access to the model container.

## 1. Find the published image

CI publishes one image per push to `feat/runai-adapter`:

```bash
gh api repos/edwardsp/azcp/actions/workflows/runai-adapter-image.yml/runs \
   --jq '.workflow_runs[0] | {status, conclusion, head_sha, html_url}'
```

The image tag follows the same convention as `azcp-cluster`:

```
ghcr.io/edwardsp/azcp/azcp-runai-adapter:feat-runai-adapter-<sha12>
```

Substitute `<sha12>` with the first 12 chars of the green build's commit.

## 2. Import the image into enroot (one-time per cluster)

On any node with shared-FS write access:

```bash
TAG="feat-runai-adapter-<sha12>"
SQSH="/shared/images/azcp-runai-adapter-${TAG}.sqsh"

mkdir -p "$(dirname "$SQSH")"
enroot import -o "$SQSH" \
    "docker://ghcr.io/edwardsp/azcp/azcp-runai-adapter:${TAG}"

ls -lh "$SQSH"
```

The squashfs is ~6-8 GiB (NGC PyTorch base + RunAI + azcp). Import takes
3-10 min depending on network.

## 3. Generate the shardlist (one-time per model)

The shardlist is a TSV manifest of `<name>\t<size>\t<last-modified>` rows.
Every rank reads the same file; each rank takes its `i/N` slice.

Pick whichever model you want to benchmark. Llama-3-70B (~140 GiB) is a
reasonable 16-node test; smaller models will be download-dominated.

Option A — generate from a login node with a local `azcp` binary:

```bash
# Auth: SAS token in URL is simplest; AAD via `az login` also works.
export AZURE_STORAGE_ACCOUNT=...
export AZURE_STORAGE_KEY=...        # or AZURE_STORAGE_SAS_TOKEN, or `az login`

bash examples/runai-adapter/generate_shardlist.sh \
    https://<acct>.blob.core.windows.net/<container>/<prefix>/ \
    /shared/manifests/<model>.shardlist
```

Option B — generate inside the container on a single node (no local azcp):

```bash
srun --nodes=1 --ntasks=1 \
     --container-image="$SQSH" --container-mounts=/shared:/shared \
     --export=ALL,AZURE_STORAGE_ACCOUNT,AZURE_STORAGE_KEY \
     azcp ls https://<acct>.blob.core.windows.net/<container>/<prefix>/ \
            --recursive --machine-readable \
            > /shared/manifests/<model>.shardlist
```

Sanity-check the manifest:

```bash
SHARDLIST=/shared/manifests/<model>.shardlist
echo "files: $(grep -cv '^#' "$SHARDLIST")"
echo "size : $(awk -F'\t' '!/^#/ && $2 != "<DIR>" {s+=$2} END {printf "%.1f GiB\n", s/1024^3}' "$SHARDLIST")"
echo "safetensors: $(grep -c '\.safetensors$' "$SHARDLIST")"
```

## 4. Provide Azure credentials to the job

`azcp` inside the container needs to authenticate. The `--export=ALL` in
the sbatch picks up your shell's environment. Easiest: set credentials
in the submitting shell before `sbatch`.

```bash
# Choose ONE:
export AZURE_STORAGE_ACCOUNT=... AZURE_STORAGE_KEY=...   # Shared Key
# or
export AZURE_STORAGE_SAS_TOKEN='?sv=...&sig=...'         # SAS
# or — if your model URI already has ?sv=... in it, no env needed.
```

Workload identity / managed identity are also supported (set
`AZURE_FEDERATED_TOKEN_FILE` + `AZURE_CLIENT_ID` + `AZURE_TENANT_ID`),
but most Slurm sites use Shared Key or SAS for simplicity.

## 5. Submit

```bash
export IMAGE="$SQSH"
export MODEL_URI="https://<acct>.blob.core.windows.net/<container>/<prefix>/"
export SHARDLIST="/shared/manifests/<model>.shardlist"
export LOCAL_CACHE="/mnt/nvme/cache-${SLURM_JOB_ID:-test}"

sbatch examples/runai-adapter/slurm/run.sbatch
```

Watch it:

```bash
JOB=$(squeue -u $USER -h -o '%i' | head -1)
tail -F azcp-runai-${JOB}.out
```

## 6. Expected output

A successful 16-node run on ND H100 v5 looks roughly like:

```
[launcher] nodes=16 head=node0(10.x.x.x) image=/shared/images/azcp-runai-adapter-...sqsh
[launcher] model_uri=https://...
[launcher] shardlist=/shared/manifests/llama3-70b.shardlist
[rank 0] download starting: azcp copy https://... /mnt/nvme/cache --recursive --shardlist ... --shard 0/16 ...
[rank 0] download done in 7.42s
[rank 0] [phase 1] download wallclock=7.42s aggregate=151.0 Gb/s (per-rank ~8.8 GiB)
[rank 0] [phase 2] global catalog: 30 files, 723 tensors
[rank 0] [phase 3] bcast wallclock=15.8s bytes=131.0 GiB throughput=71.0 Gb/s (723 tensors)
[driver] LOADED 723 tensors, 131.00 GiB, end-to-end 24.1s = 46.4 Gb/s
[validate] PASS — all 16 ranks agree on tensor hashes
Done.
```

The numbers will vary with model, NIC config, and cluster load. The
phase-3 bcast throughput is the figure to track — that's what NCCL
multi-rail tuning is meant to lift.

## 7. NCCL tuning knobs (already wired into `run.sbatch`)

| Env var | Default in sbatch | Effect |
|---|---|---|
| `NCCL_IB_HCA` | `mlx5_ib0..mlx5_ib7` | Enumerate all CX-7 rails |
| `NCCL_IB_GID_INDEX` | `3` | RoCEv2 / IB on Azure |
| `NCCL_IB_QPS_PER_CONNECTION` | `4` | Multiple QPs → multiple rails per bcast |
| `NCCL_IB_SPLIT_DATA_ON_QPS` | `1` | Stripe payload across QPs |
| `NCCL_MIN_NCHANNELS` | `32` | Enough SM channels to feed the QPs |
| `NCCL_NET_GDR_LEVEL` | `PHB` | GPU-Direct RDMA for any HCA on same PCIe host bridge |

Override any of them at submit time:

```bash
NCCL_IB_QPS_PER_CONNECTION=8 NCCL_MIN_NCHANNELS=64 \
    sbatch examples/runai-adapter/slurm/run.sbatch
```

To see what NCCL actually picked, set `NCCL_DEBUG=INFO` (verbose):

```bash
NCCL_DEBUG=INFO sbatch examples/runai-adapter/slurm/run.sbatch
grep -E 'NCCL INFO (NET|Channel|Ring)' azcp-runai-${JOB}.out | head -40
```

## 8. Troubleshooting

**Job hangs in `[phase 3]`** → NCCL likely failed to set up IB. Re-run with
`NCCL_DEBUG=INFO` and look for `NET/IB : Using interface ...` lines. If
those say `NET/Socket` instead, the IB stack isn't being picked up — check
`/dev/infiniband` mount and `NCCL_IB_HCA` matches actual device names
(`ls /sys/class/infiniband/`).

**`download done` but then `RuntimeError: ... missing from every rank's
local cache`** → `azcp` succeeded but file count doesn't match the
shardlist. Usually means the shardlist was generated against a different
prefix than `MODEL_URI`. Regenerate.

**Per-rank download <30 Gb/s** → frontend network throttling or storage
account egress limit. Check throttle counters in the azcp output (`[retry
503xN 429xM]`). Either spread across multiple storage accounts or lower
`--workers`.

**`[validate] FAIL`** → ranks disagree on tensor hashes. This is a
correctness bug in the adapter; capture full stderr from all ranks
(`grep "rank " azcp-runai-${JOB}.out`) and file an issue.

**Image pull / enroot import fails with 401** → GHCR requires a personal
access token even for public images on some sites. `enroot` reads
`~/.config/enroot/.credentials`:

```
machine ghcr.io login <github-username> password <ghcr_pat>
```

## 9. Quick smoke test (2 ranks, 1 node)

To validate the image and adapter without burning a 16-node allocation:

```bash
srun --nodes=1 --ntasks=2 --gres=gpu:2 --cpus-per-task=8 \
     --container-image="$SQSH" \
     --container-mounts=/dev/infiniband:/dev/infiniband,/mnt/nvme:/mnt/nvme,/shared:/shared \
     --export=ALL,AZURE_STORAGE_ACCOUNT,AZURE_STORAGE_KEY \
     bash -c '
       export NCCL_DEBUG=WARN
       LOCAL_CACHE=/mnt/nvme/smoke-${SLURM_PROCID}
       mkdir -p $LOCAL_CACHE
       torchrun --nnodes=1 --nproc-per-node=2 \
         --rdzv-backend=c10d --rdzv-endpoint=localhost:29500 \
         /opt/runai-adapter/test_load.py \
           --model-uri https://<acct>.blob.core.windows.net/<small-model>/ \
           --shardlist /shared/manifests/<small-model>.shardlist \
           --local-cache $LOCAL_CACHE \
           --validate
     '
```

(Per-rank `LOCAL_CACHE` because both ranks share a node — otherwise
they'd compete writing the same files.)
