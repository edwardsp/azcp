# azcp-cluster v0.4.2 — throttling characterization on 16× ND H100 v5

Date: 2026-05-15\
Cluster: 16× Azure `Standard_ND96isr_H100_v5` (single InfiniBand rail,
`UCX_NET_DEVICES=mlx5_ib0:1`)\
Image: `ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.2`\
Source: 1.2 TB / 6 × ~200 GiB blobs in a single same-region standard
storage account\
Destination: NVMe md0 (8× local NVMe striped)

`v0.4.2` adds the per-cluster retry telemetry line:

```
[cluster] retries: 503xN across M downloaders
```

Absence of the line ⇒ zero throttling. This sweep maps the operating
envelope where Azure Blob returns `503 ServerBusy` /
`OperationTimedOut` and how the available knobs (`--concurrency`,
`--parallel-files`, `--max-bandwidth`) interact with that envelope.

All runs use range-sharding (`--shard-size 2GiB`) except the no-shard
defaults baseline.

## Workload A — `--concurrency` sweep

`--concurrency` = HTTP requests in flight per active downloader rank.
With 16 nodes × 1 rank/node, total in-flight ≈ 16 × concurrency for
sharded runs (the defaults baseline only had 6 active downloaders since
the 6 files were assigned 1-per-rank without sharding).

| Variant | shard | concurrency | active DL ranks | DL Gb/s | retries (503×N) | bcast Gb/s | total wall | exit |
|---|---:|---:|---:|---:|---:|---:|---:|:---:|
| defaults | `0` | default | 6 | 144 | 0 | n/a | — | 0 |
| shardOnly | 2 GiB | default | 16 | 254 | 4 569 | 137 | 116 s | 0 |
| shard+C16 | 2 GiB | 16 | 16 | 127 | 0 | 148 | 154 s | 0 |
| shard+C32 | 2 GiB | 32 | 16 | 177 | 0 | 148 | 132 s | 0 |
| shard+C64 | 2 GiB | 64 | 16 | 262 | 3 347 | 148 | 121 s | 0 |
| shard+C128 | 2 GiB | 128 | 16 | — | — | — | — | 5 (OOM) |
| shard+C256 | 2 GiB | 256 | 16 | — | — | — | — | 5 (OOM) |

Throttling is a **hard cliff** between C32 (177 Gb/s, 0 retries) and
C64 (262 Gb/s, 3 347 retries). It corresponds to ~200 Gb/s aggregate
ingress to a single storage account in the same region — the published
Azure Blob per-account ingress soft limit ([scalability targets][msdocs]).

C128 / C256 OOM the cgroup: 16 ranks × 128+ × 16 MiB block buffers ×
2 GiB shard buffers exceeds the 1.8 TiB node memory budget once the
in-flight working set is doubled.

## Workload B — `--parallel-files` sweep

`--parallel-files` = number of files a downloader rank may stream
concurrently. Hypothesis under test: total in-flight requests =
`parallel_files × concurrency_per_file`, so `PFn × Cdef` should behave
like `C(n × def)`.

| Variant | shard | `--parallel-files` | `--concurrency` | DL Gb/s | retries (503×N) | bcast Gb/s | total wall | exit |
|---|---:|---:|---:|---:|---:|---:|---:|:---:|
| PF2_Cdef | 2 GiB | 2 | default | 233 | 66 | 150 | 119 s | 0 |
| PF4_Cdef | 2 GiB | 4 | default | 226 | 175 | 148 | 117 s | 0 |
| PF8_Cdef | 2 GiB | 8 | default | 249 | 2 015 | 138 | 119 s | 0 |
| PF2_C16 | 2 GiB | 2 | 16 | 122 | 0 | 147 | 160 s | 0 |
| PF4_C16 | 2 GiB | 4 | 16 | 113 | 0 | 135 | 168 s | 0 |

Findings:

- `--parallel-files` does scale aggregate request pressure roughly
  linearly: PF2 → 66, PF4 → 175, PF8 → 2 015 retries, the same
  monotonic growth as `--concurrency`.
- PF and C are not exactly equivalent at matched products: PF8
  (≈ C×8 if default C is small) gave 2 015 retries vs C64's 3 347,
  despite similar peak DL throughput (249 vs 262 Gb/s). PF appears to
  introduce slightly less per-account pressure per Gb/s than C,
  possibly because file-level multiplexing keeps individual request
  burst rates lower than packing many concurrent requests onto one
  file.
- Holding C=16 caps total in-flight even with PF≥2 — both variants
  stay well under threshold (122 / 113 Gb/s, 0 retries) and pay a
  wall-clock penalty.

## Workload C — `--max-bandwidth` cap

The cleanest production lever: keep the high-throughput config
(`--concurrency 64`) but bound aggregate ingress with `--max-bandwidth
200Gbps`.

| Variant | shard | concurrency | `--max-bandwidth` | DL Gb/s | retries (503×N) | bcast Gb/s | total wall | exit |
|---|---:|---:|---:|---:|---:|---:|---:|:---:|
| shard+C64 | 2 GiB | 64 | unlimited | 262 | 3 347 | 148 | 121 s | 0 |
| shard+C64+BW | 2 GiB | 64 | 200 Gbps | 196 | 0 | 148 | 122 s | 0 |

The bandwidth cap eliminates **3 347 retries at zero wall-clock cost**
— total time goes from 121 s (C64 unlimited, retries-included) to
122 s (C64 capped, clean). The cap landed on 195.77 Gb/s, just under
the 200 Gbps ceiling, confirming the limiter works as advertised.

## Conclusions

1. **Throttling threshold is ~200 Gb/s aggregate ingress** to a
   single same-region standard storage account, regardless of which
   knob drives the request pressure.
2. **`--concurrency` and `--parallel-files` both push past the
   threshold** when uncapped. Their effects are roughly additive on
   total in-flight requests but not perfectly equivalent
   retry-for-retry (PF appears slightly gentler per delivered Gb/s).
3. **`--max-bandwidth` is the right production knob.** It lets
   operators size compute (high `--concurrency`) for the worst case
   while protecting the storage account from 503s. **No measurable
   wall-clock penalty** when the cap matches the throttling
   threshold.

## Recommended production configuration

For 1.2 TB / few-large-files on 16× H100 against a single same-region
storage account:

```bash
srun --mpi=pmix azcp-cluster download <src> <dst> \
  --shard-size 2GiB \
  --concurrency 64 \
  --max-bandwidth 200Gbps
```

If `--max-bandwidth` is unavailable (older clients), fall back to
`--shard-size 2GiB --concurrency 32` (177 Gb/s, 0 retries,
+10 s wall vs the capped C64 recipe).

See [docs/handling-throttling.md](handling-throttling.md) for the
operator-facing guide that explains *why* these knobs interact this
way and how to size them for other account / cluster sizes.

[msdocs]: https://learn.microsoft.com/en-us/azure/storage/common/scalability-targets-standard-account
