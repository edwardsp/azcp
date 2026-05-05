# Changelog

All notable changes to `azcp` and `azcp-cluster` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
