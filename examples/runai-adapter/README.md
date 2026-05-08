# azcp + RunAI Model Streamer adapter

A multi-node SafeTensors loader that combines `azcp`'s sharded download
with NCCL broadcast for fan-out, with optional RunAI Model Streamer
acceleration on the owner-rank file → GPU staging.

```
                          ┌──────────────────────────┐
                          │   Azure Blob Storage     │
                          └──────────┬───────────────┘
                                     │  1× egress (sharded)
            ┌────────────┬───────────┴───────────┬────────────┐
            │            │                       │            │
       ┌────▼───┐   ┌────▼───┐  ...         ┌────▼───┐   ┌────▼───┐
       │ rank 0 │   │ rank 1 │              │rank N-2│   │rank N-1│
       │ azcp   │   │ azcp   │              │ azcp   │   │ azcp   │
       │ shard  │   │ shard  │              │ shard  │   │ shard  │
       │ 0/N    │   │ 1/N    │              │N-2/N   │   │N-1/N   │
       └────┬───┘   └────┬───┘              └────┬───┘   └────┬───┘
            │            │                       │            │
            ▼            ▼                       ▼            ▼
       ┌──────────────────────────────────────────────────────────┐
       │ NCCL per-tensor broadcast over IB/RDMA                   │
       │ owner rank streams local file → GPU via RunAI            │
       │ non-owners recv into preallocated GPU buffers            │
       └──────────────────────────────────────────────────────────┘
```

## What this is, what it isn't

**This is** the answer to "can the sharded download be used and broadcast
with RunAI Model Streamer?" — yes, by replicating azcp's deterministic LPT
in Python so every rank knows who owns what, then driving NCCL broadcast
manually after each rank loads its shard locally.

**This is not** an upstream RunAI integration. RunAI's own distributed
streamer (`is_distributed=True`) runs its own greedy LPT over chunks and
broadcasts via NCCL too. The two algorithms don't agree on assignments,
so we bypass `DistributedStreamer` and only use the single-node
`SafetensorsStreamer` for the local-file → GPU staging step.

**Compared to `azcp-cluster`** (the MPI binary): same end goal, different
slot.

| | `azcp-cluster` | this adapter |
|---|---|---|
| Process model | Standalone MPI job, populates disk | In-process, loads to GPU |
| Broadcast | MPI_Bcast over UCX/IB | NCCL over IB/NVLink |
| Output | Files on local NVMe | `state_dict` on every rank's GPU |
| Best for | inference fleets, repeated loads | one-shot training/inference startup |
| Lifecycle | Init container / Slurm prolog | Inside the trainer / vLLM worker |

## Files

| File | Purpose |
|---|---|
| `azcp_runai_loader.py` | The adapter library. Public entry: `load_sharded_state_dict()`. |
| `test_load.py` | Standalone driver: loads model, prints throughput, optional cross-rank validation. |
| `Dockerfile` | NGC PyTorch + azcp binary + RunAI streamer + adapter. |
| `generate_shardlist.sh` | One-shot helper to produce the manifest every rank consumes. |
| `aks/pytorchjob.yaml` | Kubeflow training-operator launcher (16 nodes, 1 rank/node). |
| `slurm/run.sbatch` | Slurm + enroot/pyxis launcher (16 nodes, 1 rank/node). |

## How it works

**Phase 1 — sharded download.** Each rank invokes
`azcp copy --shardlist X --shard $RANK/$WORLD ...`. azcp does deterministic
LPT bin-packing (size DESC, name ASC, greedy assign to least-loaded shard)
and downloads only this rank's slice to local NVMe. Shardlist is
pre-generated once and lives on shared storage.

**Phase 2 — header all-gather.** Each rank reads the SafeTensors JSON
headers of its owned files (~few KB each), then `dist.all_gather_object`
exchanges them. Now every rank knows the global tensor catalog without
having read any non-owned file bytes.

**Phase 3 — per-tensor NCCL broadcast.** Files iterated in stable sorted
order. The owner rank pre-loads all of its owned files in one batched
`SafetensorsStreamer.stream_files(...)` call (RunAI overlaps I/O across
files via its C++ thread pool), draining results into a name→tensor
dict. The deterministic broadcast loop then pops from that dict in
file/tensor order and `dist.broadcast`s each tensor; non-owners receive
into preallocated GPU buffers. After all files broadcast, every rank
has the full state dict on GPU.

The adapter builds the file → owner map from **filesystem ground truth**:
after `azcp` finishes, each rank scans its local cache and
`dist.all_gather_object` exchanges the file lists; the lowest-ranked
process holding each file becomes its owner. No mirroring of azcp's
internal LPT in Python — the adapter is decoupled from any future
change to azcp's sharding algorithm, and missing/corrupt downloads
raise loudly instead of being silently mis-assigned.

## Usage

### 0. Build & push the image

```bash
docker build -t ghcr.io/edwardsp/azcp/azcp-runai-adapter:latest \
    -f examples/runai-adapter/Dockerfile examples/runai-adapter
docker push ghcr.io/edwardsp/azcp/azcp-runai-adapter:latest
```

### 1. Generate the shardlist (one-time per model)

```bash
./generate_shardlist.sh \
    https://acct.blob.core.windows.net/models/llama3-70b/ \
    /shared/manifests/llama3-70b.shardlist
```

Place the output on shared storage (NFS / AzureFile PVC for AKS, Lustre
for Slurm).

### 2. Launch on 16 nodes

**AKS** (requires Kubeflow training-operator installed):

```bash
kubectl apply -f examples/runai-adapter/aks/pytorchjob.yaml
kubectl logs -f azcp-runai-load-master-0
```

**Slurm** (requires enroot + pyxis, image imported as squashfs):

See [`slurm/TESTING.md`](slurm/TESTING.md) for the full step-by-step
runbook (image discovery, enroot import, shardlist generation, NCCL
multi-rail tuning, troubleshooting). Quick version:

```bash
enroot import -o /shared/images/azcp-runai-adapter.sqsh \
    docker://ghcr.io/edwardsp/azcp/azcp-runai-adapter:latest

MODEL_URI=https://acct.blob.core.windows.net/models/llama3-70b/ \
SHARDLIST=/shared/manifests/llama3-70b.shardlist \
LOCAL_CACHE=/mnt/nvme/cache \
sbatch examples/runai-adapter/slurm/run.sbatch

tail -f azcp-runai-<jobid>.out
```

### 3. Local 2-rank smoke test (single node, 2 GPUs)

```bash
torchrun --nnodes=1 --nproc-per-node=2 \
    --rdzv-backend=c10d --rdzv-endpoint=localhost:29500 \
    test_load.py \
        --model-uri https://acct.blob.core.windows.net/models/tiny/ \
        --shardlist /tmp/tiny.shardlist \
        --local-cache /tmp/cache-$RANK \
        --validate
```

(For the smoke test, set `LOCAL_CACHE` per-rank since both ranks share a
node — otherwise they'd compete writing the same files.)

## Expected results

A successful 16-node run on ND H100 v5 prints something like:

```
[rank 0] [phase 1] download wallclock=12.3s aggregate=68.2 Gb/s (per-rank ~5.2 GiB)
[rank 0] [phase 2] global catalog: 30 files, 723 tensors
[rank 0] [phase 3] bcast wallclock=18.7s bytes=131.0 GiB throughput=60.1 Gb/s (723 tensors)
[driver] LOADED 723 tensors, 131.00 GiB, end-to-end 31.2s = 36.0 Gb/s
[validate] PASS — all 16 ranks agree on tensor hashes
```

Numbers will vary with model size, file count, NIC config, NCCL tuning.
The bcast throughput in phase 3 is per-tensor (not coalesced), so models
with many small tensors will see lower per-tensor numbers than aggregate
fabric capacity. See "Production notes" at the bottom of
`azcp_runai_loader.py` for coalescing.

## Tuning knobs

| Where | Knob | Effect |
|---|---|---|
| `test_load.py` | `--workers` | azcp tokio runtimes per rank. 4 reaches ~64 Gb/s on H100 v5. |
| `test_load.py` | `--concurrency` | per-runtime in-flight HTTP block requests. |
| `test_load.py` | `--block-size` | HTTP range size. 16 MiB matches azcp defaults. |
| `test_load.py` | `--no-runai` | Bypass RunAI streamer; use mmap fallback. |
| Container env | `NCCL_IB_HCA` | Which IB NICs NCCL uses for broadcast. |
| Container env | `NCCL_IB_GID_INDEX` | RoCE vs IB GID selection. 3 is RoCEv2 / IB on Azure. |
| Container env | `NCCL_DEBUG=INFO` | Verbose NCCL init logging if broadcast hangs. |

## Known limitations / next steps

- **Per-tensor broadcast overhead.** Each `dist.broadcast` is one NCCL
  call. For a 10k-tensor model, that's 10k × ~50us launch overhead =
  ~0.5s of pure scheduling on top of the bandwidth-limited transfer.
  Coalesce with `torch.distributed._coalescing_manager` or batch by file
  for production. See bottom of `azcp_runai_loader.py`.
- **One rank per node assumption.** Multi-rank-per-node is sketched in the
  same notes section but not implemented. For DDP/TP, run one rank per
  GPU but only have local_rank 0 download.
- **No tensor-parallel slicing.** Every rank gets every tensor in full.
  For TP layouts, replace `dist.broadcast` with `dist.scatter` over a
  TP-aware partition.
- **Owner pre-materialization memory cost.** The owner rank loads ALL of
  its owned tensors onto GPU in one batched RunAI call before broadcast
  begins (so RunAI's thread pool can overlap I/O across files). For a
  16-way shard of an LLM checkpoint this is typically 2-20 GiB / rank,
  comfortable on H100 80 GB. For models that don't fit, shard finer
  (raise world size) or revert to a per-file load loop.
- **Globally-unique tensor names assumed.** Sharded safetensors
  checkpoints in the wild use unique names across shard files; the
  batched `_load_owned_files` returns one flat name→tensor dict and
  silently overwrites on collision. If you encounter a checkpoint that
  reuses tensor names across shards, key by `(file, name)` instead.
- **RunAI Azure backend.** RunAI Model Streamer's Azure plugin was in
  flight as of 0.13.x. This adapter sidesteps the question by having
  azcp do the Azure download and only using RunAI for local-file
  staging — which works with stock `runai-model-streamer` (no extra
  backend wheel needed).
