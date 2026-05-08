"""
azcp + RunAI Model Streamer adapter — multi-node SafeTensors loader.

Each rank downloads 1/N of the SafeTensors files via `azcp copy --shard i/N`,
then NCCL-broadcasts each tensor from its owner rank so every rank ends up
with the full state dict. RunAI Model Streamer is used on the owner rank to
stream local files into pinned/GPU memory with overlapped I/O.

Why this layout:

- After `azcp` download, each rank scans its local cache and all-gathers
  the file list. The owner-of-file map is built from filesystem ground
  truth (lowest rank wins ties). No need to mirror azcp's LPT in Python —
  the adapter is decoupled from any future change to azcp's sharding.
- Per-tensor broadcast is sequential per-call but works on any
  torch.distributed backend. For production, coalesce with
  `torch.distributed._coalescing_manager` (see notes at bottom of file).
- The adapter assumes one rank per node. Multi-GPU per node should be
  handled at a higher layer (DDP / TP / PP after load).

Tested against:
- runai-model-streamer >= 0.13.0
- torch >= 2.4 with NCCL
- azcp >= 0.3.0

Public API:
    load_sharded_state_dict(model_uri, local_cache, shardlist_path) -> dict
"""

from __future__ import annotations

import json
import os
import shutil
import struct
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import torch


# --------------------------------------------------------------------------- #
# 1. Shardlist parsing + post-download owner discovery.
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class FileEntry:
    """One row of an azcp shardlist (TSV: name\\tsize\\tlast_modified)."""

    name: str
    size: int


def parse_shardlist(path: str) -> list[FileEntry]:
    """Read an `azcp ls --machine-readable` TSV. Skips comments / <DIR> rollups."""
    out: list[FileEntry] = []
    with open(path, "r", encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            parts = line.split("\t")
            if len(parts) < 2:
                continue
            name, size_s = parts[0], parts[1]
            if size_s == "<DIR>":
                continue
            try:
                out.append(FileEntry(name=name, size=int(size_s)))
            except ValueError:
                continue
    return out


def discover_owners(
    cache_root: str,
    expected_names: set[str],
    rank: int,
    world: int,
) -> dict[str, int]:
    """
    Build the file→owner map from filesystem ground truth.

    Each rank lists the safetensors files actually present in its local
    cache (post-`azcp` download), then `dist.all_gather_object` exchanges
    those lists. The owner of each file is the lowest-ranked process that
    has it on disk — deterministic across all ranks given identical
    `expected_names`.

    Why this instead of mirroring azcp's LPT in Python:
    - Single source of truth (the filesystem) rather than a re-derived
      prediction. Survives any future azcp sharding change.
    - Catches missing/corrupt downloads loudly: any file not present on
      *any* rank raises immediately.
    - Tolerates replication for free if azcp ever fans out hot shards.

    Cost: one all_gather_object of ~|files| × ~80 B strings — KB-scale
    even at 400 ranks.
    """
    import torch.distributed as dist

    cache = Path(cache_root)
    local_files = sorted(
        p.relative_to(cache).as_posix()
        for p in cache.rglob("*.safetensors")
        if p.is_file() and p.relative_to(cache).as_posix() in expected_names
    )
    gathered: list[list[str]] = [None] * world  # type: ignore[list-item]
    dist.all_gather_object(gathered, local_files)

    owner_of_file: dict[str, int] = {}
    for r, files in enumerate(gathered):
        for f in files:
            owner_of_file.setdefault(f, r)  # lowest rank wins ties

    missing = expected_names - owner_of_file.keys()
    if missing:
        sample = sorted(missing)[:5]
        suffix = "..." if len(missing) > 5 else ""
        raise RuntimeError(
            f"[rank {rank}] {len(missing)} expected safetensors file(s) "
            f"missing from every rank's local cache after azcp download: "
            f"{sample}{suffix}"
        )
    return owner_of_file


# --------------------------------------------------------------------------- #
# 2. SafeTensors header — read locally, all_gather across ranks.
# --------------------------------------------------------------------------- #


def read_safetensors_header(path: str) -> dict:
    """Returns the JSON header (tensor metadata) from a .safetensors file."""
    with open(path, "rb") as fh:
        n = struct.unpack("<Q", fh.read(8))[0]
        return json.loads(fh.read(n).decode("utf-8"))


_DTYPE_MAP_NAMES = (
    "F64", "F32", "F16", "BF16", "I64", "I32", "I16", "I8", "U8", "BOOL",
)


def st_dtype_to_torch(s: str) -> "torch.dtype":
    import torch
    table = {
        "F64": torch.float64, "F32": torch.float32, "F16": torch.float16,
        "BF16": torch.bfloat16, "I64": torch.int64, "I32": torch.int32,
        "I16": torch.int16, "I8": torch.int8, "U8": torch.uint8,
        "BOOL": torch.bool,
    }
    return table[s]


# --------------------------------------------------------------------------- #
# 3. Adapter entry point.
# --------------------------------------------------------------------------- #


def _log(rank: int, msg: str) -> None:
    print(f"[rank {rank}] {msg}", flush=True, file=sys.stderr)


def _run_azcp_shard(
    model_uri: str,
    local_cache: str,
    shardlist_path: str,
    rank: int,
    world: int,
    workers: int,
    concurrency: int,
    block_size: int,
) -> None:
    """Invoke `azcp copy --shardlist X --shard rank/world` for this rank."""
    cmd = [
        "azcp",
        "copy",
        model_uri,
        local_cache,
        "--recursive",
        "--shardlist",
        shardlist_path,
        "--shard",
        f"{rank}/{world}",
        "--workers",
        str(workers),
        "--concurrency",
        str(concurrency),
        "--block-size",
        str(block_size),
        "--no-progress",
    ]
    _log(rank, f"download starting: {' '.join(cmd)}")
    t0 = time.monotonic()
    subprocess.run(cmd, check=True)
    dt = time.monotonic() - t0
    _log(rank, f"download done in {dt:.2f}s")


def _local_path(local_cache: str, blob_name: str) -> str:
    """`azcp copy <prefix> <dst>` strips the source prefix; we mirror that.

    For our manifest (rooted at the model prefix), files land directly under
    `local_cache/`. If you change the source URL to point deeper or shallower,
    adjust here.
    """
    return os.path.join(local_cache, blob_name)


def load_sharded_state_dict(
    model_uri: str,
    local_cache: str,
    shardlist_path: str,
    *,
    device: str | None = None,
    workers: int = 4,
    concurrency: int = 32,
    block_size: int = 16 * 1024 * 1024,
    keep_local: bool = True,
    use_runai_streamer: bool = True,
) -> "dict[str, torch.Tensor]":
    import torch
    import torch.distributed as dist
    if not dist.is_initialized():
        raise RuntimeError("torch.distributed must be initialized before calling")
    rank = dist.get_rank()
    world = dist.get_world_size()
    if device is None:
        device = f"cuda:{torch.cuda.current_device()}"
    torch.cuda.set_device(device)

    # -- Phase 1: download this rank's shard -------------------------------- #
    Path(local_cache).mkdir(parents=True, exist_ok=True)
    entries = parse_shardlist(shardlist_path)
    safetensors_entries = [e for e in entries if e.name.endswith(".safetensors")]
    if not safetensors_entries:
        raise RuntimeError(f"no .safetensors files in {shardlist_path}")

    t_dl0 = time.monotonic()
    _run_azcp_shard(
        model_uri, local_cache, shardlist_path, rank, world,
        workers=workers, concurrency=concurrency, block_size=block_size,
    )
    dist.barrier()
    t_dl = time.monotonic() - t_dl0
    if rank == 0:
        total_bytes = sum(e.size for e in safetensors_entries)
        gbps = (total_bytes * 8) / (t_dl * 1e9)
        _log(rank, f"[phase 1] download wallclock={t_dl:.2f}s "
                   f"aggregate={gbps:.1f} Gb/s "
                   f"(per-rank ~{total_bytes/(world*1024**3):.1f} GiB)")

    # -- Phase 1b: post-download owner discovery (filesystem ground truth) -- #
    expected_names = {e.name for e in safetensors_entries}
    owner_of_file = discover_owners(local_cache, expected_names, rank, world)

    # -- Phase 2: all_gather safetensors headers ---------------------------- #
    my_headers: dict[str, dict] = {}
    for fe in safetensors_entries:
        if owner_of_file[fe.name] == rank:
            my_headers[fe.name] = read_safetensors_header(_local_path(local_cache, fe.name))
    gathered: list[dict] = [None] * world  # type: ignore[list-item]
    dist.all_gather_object(gathered, my_headers)
    global_headers: dict[str, dict] = {}
    for d in gathered:
        global_headers.update(d)
    if rank == 0:
        n_tensors = sum(
            sum(1 for k in h if k != "__metadata__") for h in global_headers.values()
        )
        _log(rank, f"[phase 2] global catalog: "
                   f"{len(global_headers)} files, {n_tensors} tensors")

    # -- Phase 3: per-tensor NCCL broadcast --------------------------------- #
    state: dict[str, "torch.Tensor"] = {}
    streamer = None
    if use_runai_streamer:
        try:
            from runai_model_streamer.safetensors_streamer.safetensors_streamer import (
                SafetensorsStreamer,
            )
            streamer = SafetensorsStreamer()
            streamer.__enter__()
        except ImportError:
            if rank == 0:
                _log(rank, "runai_model_streamer not installed; using fallback")
            streamer = None

    # Stable order — every rank iterates the same list.
    file_order = sorted(global_headers.keys())
    t_bc0 = time.monotonic()
    bytes_broadcast = 0

    # Pre-load ALL owned tensors in one batched call. RunAI overlaps I/O
    # across files via its C++ thread pool; get_tensors() returns in
    # completion order, so we drain into a name→tensor dict and pop from
    # it in the deterministic broadcast loop below. Memory: owner holds
    # all owned tensors on GPU before bcast (typ. 2-20 GiB / rank for a
    # 16-way LLM shard, fits H100 80 GB). Shard finer if it doesn't fit.
    owned_paths = [
        _local_path(local_cache, fname)
        for fname in file_order
        if owner_of_file[fname] == rank
    ]
    owned_headers = {
        _local_path(local_cache, fname): global_headers[fname]
        for fname in file_order
        if owner_of_file[fname] == rank
    }
    owner_pool = _load_owned_files(owned_paths, owned_headers, device, streamer)

    for fname in file_order:
        owner = owner_of_file[fname]
        header = global_headers[fname]
        tensor_names = sorted(k for k in header if k != "__metadata__")

        for tname in tensor_names:
            meta = header[tname]
            shape = tuple(meta["shape"])
            dtype = st_dtype_to_torch(meta["dtype"])
            if rank == owner:
                t = owner_pool.pop(tname)
                if t.dtype != dtype or str(t.device) != device:
                    t = t.to(device=device, dtype=dtype, non_blocking=True)
            else:
                t = torch.empty(shape, dtype=dtype, device=device)
            dist.broadcast(t, src=owner)
            state[tname] = t
            bytes_broadcast += t.element_size() * t.numel()

    if owner_pool:
        raise RuntimeError(
            f"[rank {rank}] {len(owner_pool)} owned tensor(s) not consumed by "
            f"broadcast loop — header/file mismatch: {sorted(owner_pool)[:5]}"
        )

    if streamer is not None:
        streamer.__exit__(None, None, None)

    torch.cuda.synchronize()
    t_bc = time.monotonic() - t_bc0
    if rank == 0:
        gbps = (bytes_broadcast * 8) / (t_bc * 1e9)
        _log(rank, f"[phase 3] bcast wallclock={t_bc:.2f}s "
                   f"bytes={bytes_broadcast/1024**3:.1f} GiB "
                   f"throughput={gbps:.1f} Gb/s "
                   f"({len(state)} tensors)")

    # -- Phase 4: optional cleanup ----------------------------------------- #
    if not keep_local:
        for fname in file_order:
            if owner_of_file[fname] == rank:
                try:
                    os.unlink(_local_path(local_cache, fname))
                except FileNotFoundError:
                    pass

    return state


def _load_owned_files(
    paths: list[str],
    headers_by_path: dict[str, dict],
    device: str,
    streamer,
) -> "dict[str, torch.Tensor]":
    """
    Load every locally-owned safetensors file into a single name→tensor dict.

    With RunAI: one `stream_files(paths, ...)` call lets the C++ thread
    pool overlap I/O across all owned files. Without RunAI: mmap fallback
    loads files sequentially (slower but no extra deps).

    Tensor names are assumed globally unique across sharded safetensors
    files (standard sharded-checkpoint convention). Returned dict is then
    drained by the broadcast loop in deterministic file/tensor order.
    """
    import torch

    if not paths:
        return {}

    if streamer is not None:
        streamer.stream_files(paths, device=device, is_distributed=False)
        return {name: tensor for name, tensor in streamer.get_tensors()}

    import mmap
    out: dict[str, "torch.Tensor"] = {}
    for path in paths:
        header = headers_by_path[path]
        with open(path, "rb") as fh:
            n = struct.unpack("<Q", fh.read(8))[0]
            data_start = 8 + n
            mm = mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ)
            for tname, meta in header.items():
                if tname == "__metadata__":
                    continue
                offset = data_start + meta["data_offsets"][0]
                length = meta["data_offsets"][1] - meta["data_offsets"][0]
                shape = tuple(meta["shape"])
                dtype = st_dtype_to_torch(meta["dtype"])
                buf = bytes(mm[offset : offset + length])
                t = torch.frombuffer(buf, dtype=dtype).reshape(shape)
                out[tname] = t.to(device, non_blocking=True)
            mm.close()
    return out


# --------------------------------------------------------------------------- #
# Production notes
# --------------------------------------------------------------------------- #
#
# 1. Coalesced broadcast.  Per-tensor dist.broadcast() makes one NCCL call per
#    tensor, which adds ~50us overhead × N tensors.  For large state dicts
#    (Llama-3-70B has ~700 tensors, DeepSeek-V3 ~10k), that's measurable.
#    Replace the inner loop with:
#
#        with torch.distributed._coalescing_manager(group=None, async_ops=False):
#            for tname in tensor_names:
#                ...
#                dist.broadcast(t, src=owner)
#
#    or batch all tensors of one file into a flat byte buffer, broadcast once,
#    then split.  We benchmark per-tensor as the upper bound on overhead.
#
# 2. Multi-rank-per-node.  This adapter assumes 1 rank/node so the local cache
#    is sized once per node.  For 1-rank-per-GPU (DDP), have only local_rank=0
#    on each node call _run_azcp_shard, then have other local ranks read the
#    same files via shared local FS.  The owner_of_file map should then key on
#    NODE rank, not GPU rank, and the broadcast source should be local_rank=0
#    of the owning node.
#
# 3. Tensor-parallel sharding.  If you have a TP layout, the broadcast above
#    is wasteful — every rank only needs its TP slice.  Replace dist.broadcast
#    with dist.scatter using a TP-aware partition.  Out of scope here.
