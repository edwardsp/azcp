# Changelog

All notable changes to `azcp` and `azcp-cluster` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.3.1] — 2026-05

Three independent correctness fixes shaken out by a 16× ND_H100_v5
shakedown. No new features; users of v0.3.0 in distributed launchers
or with `--bcast-chunk` ≥ 2 GiB should upgrade.

### Fixed

#### `azcp` (single-node)

- **`--workers` no longer overrides a user-supplied `--shard`.** Previously,
  passing `--shard X/N` alongside `--workers W>1` silently dropped the
  outer shard and split only across the W workers, so every rank in a
  distributed launcher (MPI, torchrun, Slurm) downloaded the full file
  set instead of its `1/N` slice. The two flags now compose: worker `i`
  of process `X` owns sub-partition `(X*W + i)/(N*W)`, giving `N*W` total
  partitions across the cluster.

#### `azcp-cluster`

- **Skip directory-marker blobs in the LIST stage.** Zero-byte blobs whose
  name equals the source prefix (or ends in `/`) — created by Storage
  Explorer or `azcopy mkdir`-style tools as directory placeholders —
  mapped to an empty local-relative path. The broadcast receiver then
  tried to `OpenOptions::create` the destination directory itself,
  failing with `EISDIR (os error 21)` and aborting the job. These entries
  are now filtered once at LIST so all stages agree.
- **Sub-chunk `MPI_Ibcast` to avoid `int` count overflow.** The single
  call site cast `n as i32` silently aliased per-chunk byte counts ≥ 2 GiB:
  - `--bcast-chunk 2 GiB` → `i32::MIN` → MPI fatal abort.
  - `--bcast-chunk 4 GiB` → `0` → no-op `MPI_Ibcast` returns success but
    moves zero bytes; receivers commit uninitialized buffer contents to
    disk with no error or log warning.
  - `--bcast-chunk 8 GiB` → same no-op + page-fault thrash from the
    1 TiB combined buffer allocation.

  Each logical pipeline chunk is now split into ≤ 1 GiB `MPI_Ibcast`s and
  the buffer slot is only released / written once all of its sub-bcasts
  complete (out-of-order safe via `MPI_Waitany`). User-facing
  `--bcast-chunk` semantics are unchanged.

#### `azcp ls`

- `--machine-readable` summary footer now goes to stderr so the TSV output
  on stdout can be piped directly into `azcp copy --shardlist -` without
  a trailing junk line.

## [v0.3.0] — 2026-05

Second `azcp-cluster` performance lift (multi-writer + O_DIRECT bcast,
3.5× faster end-to-end on NVMe), an opt-in cryptographic integrity check
for the broadcast path, and a matching `--direct` write-path on the
single-node `azcp copy` CLI. Plus a rate-limiter, K-of-N download
sharding, an az-cli auth fix, and the jemalloc/mimalloc dependencies are
gone (system allocator only).

### Added

#### `azcp-cluster`

- **`--bcast-writers N`** (default `2`). Fans completed bcast chunks
  across N writer threads (dispatched by `file_id % N`) on receiver
  ranks. Owner ranks are unaffected. Lifts the single-thread-buffered-
  write bottleneck that capped the v0.2.1 receive path.
- **`--bcast-direct`** (default **on**; opt out with `--no-bcast-direct`).
  Opens output files with `O_DIRECT` and uses 4 KiB-aligned buffers; the
  trailing partial chunk of each file is rounded up to alignment for the
  write and `ftruncate`'d back to its true length at close. Use
  `--no-bcast-direct` on filesystems that don't support `O_DIRECT` (NFS,
  some FUSE mounts).
- **`--verify`**. After bcast, every rank computes MD5 of every local
  file (rayon-parallel), `MPI_Allgather`s the digests, and rank 0
  cross-checks. Where the source blob has a `Content-MD5` (typically
  only blobs uploaded as a single block), the digest is also compared
  against that. Mismatch logs the offending file and aborts with
  exit 7. Cost on the 16-node / 413 GiB / 524-file reference: ~25-30 s.
- **`--download-ranks K`**. Restricts the Azure download stage to the
  first K ranks (default: all ranks), so larger clusters can keep the
  per-account egress fan-out bounded while still bcasting to all ranks
  at fabric speed.
- `BCAST_EXTRA_ARGS` env support in `tests/cluster_bench.sh` so the
  benchmark harness can sweep the new flags without script edits.

#### `azcp` (single-node)

- **`--direct`** on `azcp copy` (Linux only, no-op elsewhere). Opens
  destination files with `O_DIRECT`, bypassing the page cache. On a
  single GB300 ARM node with raided NVMe, end-to-end throughput on a
  413 GiB / 524-file checkpoint rises from **74.2 Gbps** (buffered) to
  **91.9 Gbps** — about a **24 % uplift** — by avoiding write-back
  contention with new incoming bytes once the cache fills. See
  [docs/performance-tuning.md](docs/performance-tuning.md).
- **`--max-bandwidth RATE`** on `azcp copy`. Aggregate token-bucket
  rate limiter shared across all `--workers`. Accepts both bit-rate
  (`Gbps`, `Mbps`, …) and byte-rate (`MiB/s`, `MB/s`, …) units. Useful
  when sharing a NIC with other tenants or staying under a storage
  account quota.

### Fixed

- **Rate-limiter refill is now elapsed-time based** (#4). The previous
  implementation refilled by a fixed per-tick increment regardless of
  scheduler delay, which under-counted available tokens on busy
  runtimes and over-throttled the transfer. The bucket now refills
  proportional to wall-clock elapsed since the last refill, which
  tracks the configured rate within ~10 % over multi-second windows.
- **Az-cli Bearer preferred over scraped SharedKey**. When the only
  credential available is `az login`, `azcp` now tries the Bearer token
  (RBAC) first and falls back to the scraped account key only on
  failure. Storage accounts with `allowSharedKeyAccess=false` reject
  the scraped key with 403; the new order makes those accounts work
  out of the box.

### Performance

- **Multi-writer + O_DIRECT bcast — 3.5× faster end-to-end on NVMe.**
  On the 16-node GB300 reference cluster (413 GiB / 524 files), per-
  receiver bcast bandwidth on NVMe with the new defaults
  (`--bcast-writers 2 --bcast-direct`) rose from **28 Gb/s → 99 Gb/s**
  and end-to-end wall-clock dropped from **134 s → 48 s**. Updated
  reference table and tuning options in
  [docs/cluster-benchmarks.md](docs/cluster-benchmarks.md).
- **`--direct` on single-node `azcp copy` — +24 % on raided NVMe.**
  See above.

### Changed

- **Dropped jemalloc and mimalloc** (#6). The `jemalloc` / `mimalloc`
  Cargo features and their `#[global_allocator]` blocks are removed;
  `azcp` and `azcp-cluster` now ship with the system allocator only.
  This was needed for clean operation on the GB300 64 KiB-page ARM
  kernel (where jemalloc's compiled-in 4 KiB page assumption aborted
  at startup) and removes a chunk of unsafe FFI from the dependency
  graph at no measurable throughput cost.

### Documentation

- `docs/cluster-benchmarks.md` rewritten as outline-of-options +
  measured numbers (drops the v0.2.0/v0.2.1 diagnostic arc, kept in
  git history).
- `docs/performance-tuning.md` leads with the **current single-node
  GB300 baseline** (buffered vs `--direct` on raided NVMe), then frames
  the rest of the doc as the deeper download-path investigation
  (`--discard` measurements, hostNetwork, NUMA, multi-process).
- `docs/cluster-aks.md` post-bcast verification now points at
  `--verify` (per-rank MD5 + `MPI_Allgather`) instead of suggesting an
  ad-hoc spot-check md5.
- `README.md` clarifies the credential resolution order: SAS in URL is
  rank 1 (not workload identity), and the az-cli row notes that Bearer
  is preferred over scraped SharedKey since accounts with
  `allowSharedKeyAccess=false` reject the latter with 403.
- `examples/aks/` and `examples/slurm/` gain
  download-then-train init-container patterns.

### Notes

No backwards-incompatible CLI changes. Existing scripts continue to
work; the only behavioural change is that **`azcp-cluster` bcast
output now uses `O_DIRECT` by default** — if your destination
filesystem doesn't support `O_DIRECT`, add `--no-bcast-direct`. The
`azcp copy --direct` flag is opt-in and defaults to off. New images:
`ghcr.io/edwardsp/azcp/azcp-cluster:v0.3.0` and `:latest`.

## [v0.2.1] — 2026-05

Performance fix in the `azcp-cluster` broadcast receive loop. Same
binary surface, same image tag scheme, same deployment story — just
faster, and dramatically so on NVMe destinations.

### Performance

- **Async writes in the bcast receive loop — 2.7× faster end-to-end on
  NVMe.** Writes are now handed off to a dedicated writer thread per
  `run()` instead of being executed synchronously on the main MPI
  thread between `MPI_Waitany` and the next `MPI_Ibcast` post. On the
  16-node GB300 reference cluster (413 GB / 524 files), per-receiver
  bcast bandwidth on NVMe rose from **20.26 Gb/s → 76.27 Gb/s**
  (`--bcast-chunk 512M --bcast-pipeline 16`), and end-to-end wall-clock
  dropped from **176 s → 64 s**. The new NVMe ceiling exceeds the
  previous tmpfs ceiling — the disk array was never the limit; the
  serialization between MPI receive and disk write was. Full diagnostic
  story and updated reference table in
  [docs/cluster-benchmarks.md](docs/cluster-benchmarks.md).

### Fixed

- **Write ordering correctness.** `MPI_Waitany` does not guarantee
  completions in posting order; the previous synchronous `write_all`
  happened to write in completion order and got away with it because
  tree-algorithm completions usually arrive in order. With the deeper
  parallelism enabled by async writes, in-flight chunks now carry an
  explicit byte offset and the writer uses `FileExt::write_all_at`
  (positional `pwrite`) so correctness is independent of completion
  ordering.
- **Zero-byte file fast-path.** Empty files are no longer allocated a
  pipeline slot or held open across the writer channel; they're created
  and closed inline on both owner and receiver paths.
- **Close-time errors surfaced.** The writer calls `f.sync_all()` on
  `CloseFile` and the first error encountered is propagated back
  through the writer-result channel rather than being silently dropped.

### Notes

The fix is in `crates/azcp-cluster/src/stages/broadcast.rs` (~210-line
diff against v0.2.0). No CLI flags changed. Existing scripts and
manifests work unchanged against the v0.2.1 image:
`ghcr.io/edwardsp/azcp/azcp-cluster:v0.2.1`.

## [v0.2.0] — 2026-05

First public-launch release. Adds the `azcp-cluster` companion binary for
multi-node broadcast downloads, plus the documentation and example set
needed to deploy it on AKS and Slurm.

### Added

- **`azcp-cluster`** — new companion MPI binary that downloads `1/N` of a
  dataset per rank and broadcasts it over RDMA/InfiniBand (UCX) or TCP to
  every other rank. Pays Azure egress once per byte instead of once per
  node. 5-10× faster end-to-end than naive per-node downloads on 10+
  nodes with a fast fabric.
  - Stages: `[list]` → `[diff]` → `[download]` → `[bcast]` → `[filelist]` → `[total]`.
  - `--compare {none,size,filelist}` skip-policy with `--save-filelist` /
    `--filelist` for incremental reruns.
  - `--bcast-chunk` / `--bcast-pipeline` knobs for fabric-specific tuning.
- Multi-arch (`linux/amd64`, `linux/arm64`) container at
  `ghcr.io/edwardsp/azcp/azcp-cluster:v0.2.0` and `:latest`. Published by
  the new `.github/workflows/cluster-image.yml` workflow.
- `docs/` — long-form documentation:
  - `cluster.md`, `cluster-aks.md`, `cluster-slurm.md`,
    `cluster-benchmarks.md`, `performance-tuning.md`.
- `examples/` — copy-paste deployment manifests:
  - `aks/mpi-operator-job.yaml` (recommended), `aks/statefulset-fallback.yaml`,
    `aks/install-mpi-operator.md`.
  - `slurm/enroot-import.sh`, `slurm/azcp-cluster.sbatch`,
    `slurm/apptainer.sbatch`.
- `tests/AKS_TESTING.md` — manual end-to-end AKS smoke procedure.
- `tests/cluster_smoke.sh` — single-node CLI smoke test.

### Performance

- **RDMA/UCX broadcast: ~2.65× faster than TCP** on 16-node InfiniBand.
  Measured `[bcast] BW` of 20.26 Gb/s per receiver vs 7.64 Gb/s on TCP, with
  `[total]` time dropping from 446 s to 176 s on a 413 GB / 524-file
  dataset. (Per-receiver NVMe bandwidth was further improved to 76 Gb/s
  in v0.2.1.) Reference numbers and methodology in
  [docs/cluster-benchmarks.md](docs/cluster-benchmarks.md).
- **`ibverbs-providers` fix** — image now pulls the rdma-core providers
  (mlx5 etc.) explicitly, so RDMA works out of the box on Azure
  InfiniBand SKUs without manual provider installation.

### Changed

- README rewritten around the **three throughput ceilings** (single
  process, single node, single cluster) instead of the previous "Rust port
  of azcopy" framing. Long-form perf-tuning content moved to
  `docs/performance-tuning.md`; `azcp-cluster` reference moved to
  `docs/cluster.md`.
- Test/example destination paths renamed from `deepseek` to the generic
  `dataset` across `k8s/azcp-cluster-test.yaml`, `tests/AKS_TESTING.md`,
  and the internal `.sisyphus/aks/*.yaml` manifests.

### Notes

This is the first release where the cluster path is documented for public
consumption. The `azcp` (single-node) CLI is unchanged behavior-wise from
v0.1.16; the bump to 0.2.0 reflects the addition of `azcp-cluster` as a
shipped artifact rather than any breaking change to `azcp`.

## Earlier versions

See the [GitHub Releases page](https://github.com/edwardsp/azcp/releases)
for v0.1.x release notes.
