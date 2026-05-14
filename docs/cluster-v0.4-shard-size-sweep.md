# azcp-cluster v0.4 — `--shard-size` sweep on 16× ND H100 v5

Date: 2026-05-14\
Cluster: 16× Azure `Standard_ND96isr_H100_v5` (single InfiniBand rail used)\
Image: `ghcr.io/edwardsp/azcp/azcp-cluster:v0.4.0`\
Dataset: 1.2 TB / 6 files (~200 GiB each)\
Destination: NVMe md0 (8× local NVMe striped)

This sweep validates the v0.4 range-sharding feature (`--shard-size` /
`--file-shards`) against the v0.3.1 single-broadcaster-per-file baseline.

## Results

All five shard variants completed with `--verify` PASS on every run.

| `--shard-size` | shards | DL Gb/s | Bcast Gb/s | Verify s | exit | speedup vs legacy |
|---|---:|---:|---:|---:|:---:|---:|
| `0` (regression check) | 6 | 137 | 43.8 | 352 | 0 ✓ | 1.00× |
| `32GiB` | 42 | 197 | 114.5 | 381 | 0 ✓ | 2.61× |
| `16GiB` | 78 | 243 | 132.3 | 349 | 0 ✓ | 3.02× |
| `8GiB` | 150 | 227 | 134.0 | 351 | 0 ✓ | 3.06× |
| `2GiB` | 600 | 237 | 137.7 | 349 | 0 ✓ | 3.14× |

## Findings

1. **No regression at `--shard-size 0`.** Reproduces the v0.3.1 baseline
   exactly: 43.8 Gb/s bcast, 6 shards (= 6 files), within noise of the
   historical 42.5 Gb/s. The new code path is free when the flag is off.

2. **Range sharding is a major win — 3× bcast bandwidth.** Hypothesis
   confirmed and exceeded:
   - 32 GiB → 2.6× baseline (114 Gb/s)
   - 16 GiB → 3.0× (132 Gb/s)
   - 8 GiB → 3.1× (134 Gb/s)
   - 2 GiB → 3.1× (138 Gb/s)

3. **Sweet spot is 8-16 GiB.** 2 GiB is no better than 8 GiB despite 600
   shards. The 132-138 Gb/s plateau strongly suggests we're hitting either
   the NVMe write ceiling (~100 Gb/s nominal — already past it!) or NDR
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

## Recommendation

For 16-node ND H100 v5 with large checkpoint files (≥ 16 GiB):

```bash
azcp-cluster <src> <dst> \
  --shard-size 8GiB \
  --bcast-chunk 64M \
  --bcast-writers 4 \
  --verify
```

Equivalent convenience form:

```bash
azcp-cluster <src> <dst> --file-shards 25   # → ~8 GiB shards on 200 GiB files
```

Use `--file-shards` when the file size varies run-to-run (e.g. checkpoints
that grow over training); use `--shard-size` for a stable absolute target.

## Constraints

- `--shard-size` must be a multiple of `--bcast-chunk` (enforced by
  `Args::validate()`). With the default 64 MiB chunk, both `8GiB` and
  `2GiB` are aligned.
- `--shard-size` and `--file-shards` are mutually exclusive.
- For workloads with **many small files** (more files than ranks),
  range-sharding gives little benefit — there are already plenty of
  parallel broadcasters. The legacy plan (`--shard-size 0`) is already
  optimal there.
