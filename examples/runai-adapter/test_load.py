"""
Standalone test driver for the azcp + RunAI adapter.

Loads a SafeTensors model across world_size ranks, validates that every rank
ends up with byte-identical tensors, and prints download + bcast throughput.

Launch (per environment):

    Local 2-GPU smoke:
        torchrun --nproc-per-node=1 --nnodes=2 --rdzv-backend=c10d \\
                 --rdzv-endpoint=localhost:29500 \\
                 test_load.py --model-uri ... --shardlist ... --local-cache /tmp/c

    AKS PyTorchJob:  see aks/pytorchjob.yaml
    Slurm:           see slurm/run.sbatch
"""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
import time

import torch
import torch.distributed as dist

from azcp_runai_loader import load_sharded_state_dict


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--model-uri", required=True,
                   help="Azure blob prefix, e.g. https://acct.blob.../ctr/model/")
    p.add_argument("--shardlist", required=True,
                   help="Path to shardlist (output of azcp ls --machine-readable)")
    p.add_argument("--local-cache", required=True,
                   help="Per-node local NVMe path")
    p.add_argument("--workers", type=int, default=4)
    p.add_argument("--concurrency", type=int, default=32)
    p.add_argument("--block-size", type=int, default=16 * 1024 * 1024)
    p.add_argument("--validate", action="store_true",
                   help="Cross-rank tensor hash check (slow on huge models)")
    p.add_argument("--no-runai", action="store_true",
                   help="Skip RunAI streamer; use mmap fallback instead")
    p.add_argument("--keep-local", action="store_true", default=True)
    args = p.parse_args()

    backend = "nccl" if torch.cuda.is_available() else "gloo"
    dist.init_process_group(backend=backend)
    rank = dist.get_rank()
    world = dist.get_world_size()

    if torch.cuda.is_available():
        local_rank = int(os.environ.get("LOCAL_RANK", "0"))
        torch.cuda.set_device(local_rank)
        device = f"cuda:{local_rank}"
    else:
        device = "cpu"

    if rank == 0:
        print(f"[driver] world_size={world} backend={backend} device={device}",
              flush=True, file=sys.stderr)
        print(f"[driver] model_uri={args.model_uri}", flush=True, file=sys.stderr)
        print(f"[driver] shardlist={args.shardlist}", flush=True, file=sys.stderr)
        print(f"[driver] local_cache={args.local_cache}", flush=True, file=sys.stderr)

    t_total0 = time.monotonic()
    state = load_sharded_state_dict(
        model_uri=args.model_uri,
        local_cache=args.local_cache,
        shardlist_path=args.shardlist,
        device=device,
        workers=args.workers,
        concurrency=args.concurrency,
        block_size=args.block_size,
        keep_local=args.keep_local,
        use_runai_streamer=not args.no_runai,
    )
    dist.barrier()
    t_total = time.monotonic() - t_total0

    total_bytes = sum(t.element_size() * t.numel() for t in state.values())
    if rank == 0:
        gbps = (total_bytes * 8) / (t_total * 1e9)
        print(
            f"[driver] LOADED {len(state)} tensors, "
            f"{total_bytes / 1024**3:.2f} GiB, "
            f"end-to-end {t_total:.2f}s = {gbps:.1f} Gb/s",
            flush=True,
        )

    if args.validate:
        validate_cross_rank(state, rank, world)

    dist.destroy_process_group()
    return 0


def validate_cross_rank(
    state: dict[str, torch.Tensor], rank: int, world: int,
) -> None:
    """Allreduce a per-tensor hash; assert every rank computed the same value."""
    if rank == 0:
        print(f"[validate] hashing {len(state)} tensors on rank 0...",
              flush=True, file=sys.stderr)

    h = hashlib.sha256()
    for name in sorted(state):
        t = state[name].detach().to("cpu").contiguous()
        h.update(name.encode())
        h.update(t.numpy().tobytes())
    digest = h.digest()
    digest_t = torch.tensor(list(digest), dtype=torch.uint8)
    if torch.cuda.is_available():
        digest_t = digest_t.cuda()

    gathered = [torch.zeros_like(digest_t) for _ in range(world)]
    dist.all_gather(gathered, digest_t)

    if rank == 0:
        ref = gathered[0].cpu().tolist()
        ok = all(g.cpu().tolist() == ref for g in gathered)
        if ok:
            print(f"[validate] PASS — all {world} ranks agree on tensor hashes",
                  flush=True)
        else:
            print(f"[validate] FAIL — ranks disagree on tensor hashes!",
                  flush=True)
            for i, g in enumerate(gathered):
                print(f"  rank {i}: {bytes(g.cpu().tolist()).hex()[:32]}...",
                      flush=True)
            raise SystemExit(1)


if __name__ == "__main__":
    sys.exit(main())
