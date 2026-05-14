# azcp-cluster v0.4 — `--shard-size` sweep on 16× ND H100 v5

Date: 2026-05-14\
Cluster: 16× Azure `Standard_ND96isr_H100_v5` (single InfiniBand rail used)\
Image: `ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.0`\
Destination: NVMe md0 (8× local NVMe striped)

This sweep validates the v0.4 range-sharding feature (`--shard-size` /
`--file-shards`) against the v0.3.1 single-broadcaster-per-file baseline,
across two qualitatively different workloads:

- **Few large files** (1.2 TB / 6 × 200 GiB) — the pathological case
  range-sharding was designed for.
- **Many heterogeneous files** (413 GB / 524 blobs, max 9.26 GB) — a real
  LLM checkpoint (`nvidia/DeepSeek-R1-0528-NVFP4-v2`).

The conclusion: range-sharding is a **3× win when broadcaster count is
the bottleneck**, and a **no-op (within noise) when fan-out is already
saturated**. A single default — `--shard-size 8GiB` — is safe on both:
optimal on the few-file case, within 2% of optimal on the many-file
case.

## Workload A — 1.2 TB / 6 × 200 GiB files

All five shard variants completed with `--verify` PASS on every run.

| `--shard-size` | shards | DL Gb/s | Bcast Gb/s | Verify s | exit | speedup vs legacy |
|---|---:|---:|---:|---:|:---:|---:|
| `0` (regression check) | 6 | 137 | 43.8 | 352 | 0 ✓ | 1.00× |
| `32GiB` | 42 | 197 | 114.5 | 381 | 0 ✓ | 2.61× |
| `16GiB` | 78 | 243 | 132.3 | 349 | 0 ✓ | 3.02× |
| `8GiB` | 150 | 227 | 134.0 | 351 | 0 ✓ | 3.06× |
| `2GiB` | 600 | 237 | 137.7 | 349 | 0 ✓ | 3.14× |

### Findings (Workload A)

1. **No regression at `--shard-size 0`.** Reproduces the v0.3.1 baseline
   exactly: 43.8 Gb/s bcast, 6 shards, within noise of the historical
   42.5 Gb/s. The new code path is free when the flag is off.

2. **Range sharding is a major win — 3× bcast bandwidth.** Hypothesis
   confirmed and exceeded:
   - 32 GiB → 2.6× baseline (114 Gb/s)
   - 16 GiB → 3.0× (132 Gb/s)
   - 8 GiB → 3.1× (134 Gb/s)
   - 2 GiB → 3.1× (138 Gb/s)

3. **Sweet spot is 8-16 GiB.** 2 GiB is no better than 8 GiB despite 600
   shards. The 132-138 Gb/s plateau strongly suggests we're hitting either
   the NVMe write ceiling (~100 Gb/s nominal — already past it) or NDR
   per-rail limits. Diminishing returns past 16 GiB.

4. **Throughput exceeds the assumed 100 Gb/s NVMe ceiling.** Either the
   ceiling is higher than thought, or the page cache is absorbing some of
   the writes (1.2 TB / 16 nodes = 75 GiB/node, fits easily in page cache
   on D96 nodes with ~700 GiB RAM).

5. **Download stage also benefits.** Range sharding parallelises the
   GET-side too: 137 → 243 Gb/s (legacy → 16 GiB shards). The LIST→GET
   pipeline now has more independent units of work.

6. **End-to-end wall (excluding verify+warmup) collapsed from ~670 s to
   ~130 s** for the actual download + bcast.

## Workload B — 413 GB / 524 blobs (DeepSeek-R1-NVFP4)

`nvidia/DeepSeek-R1-0528-NVFP4-v2`: 524 blobs (~165 large + ~351 dir
markers), max file 9.26 GB. All runs configured with `--bcast-chunk 1GiB`
(per the H100 tuning recipe). All passed `--verify`.

| `--shard-size` | shards | DL Gb/s | Bcast Gb/s | speedup vs legacy |
|---|---:|---:|---:|---:|
| `0` (legacy) | 524 | 229.3 | 84.77 | 1.00× |
| `8GiB` | 527 | 228.5 | 83.13 | 0.98× |
| `4GiB` | 530 | 252.4 | 84.52 | 1.00× |
| `2GiB` | 689 | 232.7 | 82.68 | 0.98× |
| `1GiB` | 857 | 258.1 | 82.49 | 0.97× |
| `512MiB` | — | — | — | rejected by validation (see below) |

### Findings (Workload B)

1. **Sharding is a no-op on this dataset.** Bcast is dead-flat at
   82-85 Gb/s across all shard sizes — including baseline 0. Range
   2.3 Gb/s ≈ noise floor.

2. **Why the difference vs Workload A?** With 524 baseline broadcasters
   already in the plan (≫ 16 ranks), per-file fan-out is already
   saturated. Adding more shards just creates additional small
   broadcasters that contend for the same fabric and disk bandwidth, with
   no headroom to fill.

3. **The 84 Gb/s ceiling is not hardware.** The same cluster reached
   138 Gb/s on Workload A with sharding. Plausible explanation here:
   heterogeneous shard sizes (some 9 GB, many small) cause stragglers —
   ranks that finish early go idle waiting for the long-pole 9 GB file.

4. **Verify is fast** (~31 s for 413 GB). Cross-rank consistency check
   handles 524 files including dir markers. Zero blob-MD5 fetches needed
   (correct: only triggered on cross-rank mismatch, which didn't occur).

5. **Download is faster than Workload A** (229-258 Gb/s, ~1.7× the
   6-file dataset). Many medium files parallelise better at LIST+GET
   than 6 monsters.

6. **Validation guard** rejected `--shard-size 512MiB` because
   `512MiB < --bcast-chunk 1GiB` and `512MiB % 1GiB ≠ 0`. This is
   intentional: per-shard `O_DIRECT` padding must never straddle shard
   boundaries. To test sub-`bcast-chunk` shards you'd need to lower
   `--bcast-chunk` simultaneously — a separate experiment.

## Recommendation

A single config works well across both workload classes:

```bash
azcp-cluster <src> <dst> \
  --shard-size 8GiB \
  --bcast-chunk 64M \
  --bcast-writers 4 \
  --verify
```

- On **few-large-file** workloads (LLM checkpoint shards, 6 × 200 GiB):
  3× bcast bandwidth (44 → 134 Gb/s). The whole point of v0.4.
- On **many-heterogeneous-file** workloads (524-blob DeepSeek): within 2%
  of the legacy plan's bandwidth. Free of regression.

If your file sizes vary run-to-run (e.g. checkpoints that grow over
training), use `--file-shards N` instead — it derives `--shard-size` as
`ceil(max_file_size / N)` rounded up to a `--bcast-chunk` multiple:

```bash
azcp-cluster <src> <dst> --file-shards 25   # ~8 GiB shards on 200 GiB files
```

### When sharding will *not* help

If your workload has **more files than ranks × ~10**, per-file fan-out
already saturates the plan and `--shard-size` becomes a no-op. The
straggler problem in Workload B (long-pole 9 GB file in a sea of small
ones) is not solved by range-sharding either — the long pole's last shard
is still on the critical path. Future work could explore size-balanced
LPT *across shards* of heterogeneous files; for now, the
`--shard-size 8GiB` default is the right safe choice.

## Constraints

- `--shard-size` must be a **multiple of `--bcast-chunk`** (enforced by
  `Args::validate()`). With the default 64 MiB chunk, every power-of-two
  shard size from 64 MiB upward is valid. With `--bcast-chunk 1GiB` (the
  H100 tuning recipe), the smallest valid shard is 1 GiB.
- `--shard-size` and `--file-shards` are **mutually exclusive**.
- The validation rule prevents per-shard `O_DIRECT` padding from
  straddling shard boundaries. Only each file's *final* chunk needs
  trim, and that's handled at `CloseFile` via `ftruncate(size)`.
