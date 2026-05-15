# Handling Azure Storage throttling (503 ServerBusy)

If your `azcp-cluster` job dies with errors that look like:

```
ERROR: model/checkpoint_NNN.distcp:
  Azure Storage (503): ServerBusy: The server is busy.
  RequestId:78e9db21-...
rank 207: download stage failed: download stage: 3 of 12 files failed
--------------------------------------------------------------------------
MPI_ABORT was invoked on rank 207 in communicator MPI_COMM_WORLD
with errorcode 5.
```

…you've saturated the storage account. Standard Azure storage accounts
have **200 Gbps egress as the default target** for most regions, and as
low as **50 Gbps in some smaller regions**
([scalability targets][msdocs]). When hundreds of ranks hit one account
simultaneously, the front-end starts replying `503 ServerBusy` /
`429 TooManyRequests` faster than the per-block retry budget can absorb,
and the job aborts.

This doc covers the three knobs `azcp-cluster` gives you to stay under
that ceiling, in order of how much they actually help, and how the
built-in retry / backoff works.

[msdocs]: https://learn.microsoft.com/en-us/azure/storage/common/scalability-targets-standard-account

---

## TL;DR

| Knob | What it does | When to use it |
|---|---|---|
| `--download-ranks K` | Caps the number of ranks pulling from Azure; the rest receive via `MPI_Bcast`. | **Always**, on any job past ~16 nodes. Best lever. |
| `--max-bandwidth RATE` | Aggregate cluster cap, divided evenly across the K downloaders. | Set just under your account's egress quota. |
| `--max-retries N` | Per-block retry budget (default 5). | Raise to **guarantee** completion under bursty throttling. Watch the cost (below). |

For a standard 200 Gbps account (anchored on the 16-node ND H100 v5
sweep — see Recipe below):

```bash
srun --mpi=pmix azcp-cluster download \
  https://acct.blob.core.windows.net/ctr/ /mnt/nvme/ \
  --download-ranks 16 \
  --max-bandwidth 200Gbps \
  --max-retries 15 \
  --shard-size 2GiB
```

---

## Why it happens

`azcp-cluster` is aggressive by design — that's how each rank reaches its
share of line rate. Per rank, defaults are 16 parallel files × 64
in-flight HTTP block requests. Multiply by hundreds of ranks and you
have **hundreds of thousands of concurrent connections to one storage
account** — well above what 200 Gbps (let alone 50 Gbps) quotas will
serve. Azure pushes back, the client retries, the retries are also
throttled, and eventually a per-block retry budget runs out.

The fix is not "retry harder" first — it's **submit less concurrent
load**. `--download-ranks` is the lever that does that without losing
end-to-end throughput, because intra-cluster `MPI_Bcast` over IB / RDMA
(~130 Gb/s on ND H100 v5) is faster than per-rank Azure egress anyway.
The slow leg is always Azure → cluster, never cluster → cluster.

---

## Knob 1: `--download-ranks K` (best lever, use it first)

Restricts the download phase to the first K ranks (rank `0..K-1`). The
remaining ranks sit idle during download and only participate in
`MPI_Bcast`. Default: all ranks download.

```bash
srun --mpi=pmix azcp-cluster download <src> <dst> --download-ranks 16
```

Why this is the right answer:

- Cuts concurrent connections to the storage account by `N/K`× — easily
  10× or 20× on big jobs.
- End-to-end wall time is unchanged because the broadcast leg already
  outpaces the download leg.
- Combined with `--max-bandwidth`, each downloader gets a deterministic
  `total / K` bytes/sec — predictable, schedulable, quota-friendly.

Sizing K: it depends on your VM's NIC speed and the storage account's
actual quota, not on a fixed table. The empirical anchor we have is the
v0.4 sweep: **16 × `Standard_ND96isr_H100_v5` nodes saturated a
standard 200 Gbps account at K = 16, hitting 237 Gb/s download**
(see [`cluster-v0.4-shard-size-sweep.md`](cluster-v0.4-shard-size-sweep.md)).
That's ~15 Gb/s per rank — well within H100 v5's NIC budget.

Scaling from there:

- **Faster VMs / higher quota** → scale K up proportionally. If you've
  had your account quota raised by Azure support, you can drive more
  ranks against it.
- **Slower NICs** → scale K up (each rank produces less, so more ranks
  are needed to fill the quota), but the diminishing return is
  per-connection overhead at the account front-end.
- If you need more aggregate throughput than one account will give you,
  the answer is either a quota increase (request via Azure support) or
  splitting your data across multiple storage accounts.

The split is deterministic: the LPT bin-packer divides the dataset into
K size-balanced shards (just as for K = N), and the broadcast plan is
computed from "who owns the file," so receiver ranks don't need to know
which subset downloaded.

---

## Knob 2: `--max-bandwidth RATE` (quota friend, with a caveat)

Hard caps the **aggregate** download rate across all active downloaders.
Internally, the cap is divided evenly: each of the K downloader ranks
gets `RATE / K` bytes/sec.

```bash
srun --mpi=pmix azcp-cluster download <src> <dst> \
  --download-ranks 32 --max-bandwidth 200Gbps
```

Accepted units: `Tbps`, `Gbps`, `Mbps`, `Kbps`, `bps` (decimal bits/s);
`TB/s`, `GB/s`, `MB/s`, `KB/s` (decimal bytes/s); `TiB/s`, `GiB/s`,
`MiB/s`, `KiB/s` (binary bytes/s). Trailing `/s` is optional. Bare
numbers are bytes/s.

### The caveat: even split, uneven work

The cap is **divided equally across all K downloaders, regardless of
how much work each one actually has.** With LPT bin-packing, sizes are
well-balanced when the workload is large files. But if you have **fewer
files than downloader ranks**, some ranks end up with zero files and
their share of the bandwidth budget is wasted — those ranks can't use it
because they have nothing to download.

Example: K = 32 downloaders, `--max-bandwidth 200Gbps`, but only 12
files in the dataset. Twenty ranks get nothing. Each of the 12 active
ranks is capped at `200 / 32 = 6.25 Gbps`, even though there's
`20 × 6.25 = 125 Gbps` of unused budget sitting on idle ranks.
Effective ceiling: ~75 Gbps instead of the 200 Gbps you asked for.

**Fix:** enable range-sharding (`--shard-size 2GiB` or
`--file-shards N`), which splits each large file into byte-range shards
and distributes them across all K ranks. With sharding, every downloader
rank has work and the bandwidth budget is fully consumed. See
[`docs/cluster.md`](cluster.md) for sharding details.

`--max-bandwidth` only applies to the **download** stage; `MPI_Bcast`
over the cluster fabric is never throttled.

---

## Knob 3: `--max-retries N` (guarantee completion, but watch the cost)

Default: **5**. Environment variable: `AZCP_MAX_RETRIES`.

Raises the per-block retry budget. Use it to **guarantee** a download
completes through bursty throttling — but always pair with
`--download-ranks` and watch the retry counters (live per-rank in the
progress bar when stderr is a TTY, or the end-of-run `[cluster] retries:
…` summary on rank 0 in non-TTY runs — see "Backoff" §3 below). If
those numbers are climbing, you're submitting too much load — fix the
load first, then trust retries to handle the residual jitter.

```bash
srun --mpi=pmix azcp-cluster download <src> <dst> \
  --download-ranks 16 --max-bandwidth 200Gbps --max-retries 30
```

### Why "watch the cost" — the math

Retries serialize per block: a block that hits its full retry budget
adds *significant* latency on top of the expected transfer time. Concrete
numbers for a 1 TiB dataset on a standard 200 Gbps account:

```
Expected download time:
  1 TiB = 1024^4 bytes = 8.80 × 10^12 bits
  at 200 Gbps = 2.0 × 10^11 bits/s
  → 44 seconds

Worst-case wait, --max-retries 5 (default):
  attempts 1→2..5→6 = 0.5 + 1 + 2 + 4 + 8 s
  → 15.5 seconds per stuck block
  → 15.5 / 44 ≈ 35% overhead

Worst-case wait, --max-retries 10:
  attempts 1→2..10→11 = 0.5 + 1 + 2 + 4 + 8 + 16 + 30 + 30 + 30 + 30 s
  → 151.5 seconds per stuck block
  → 151.5 / 44 ≈ 344% overhead (3.4× expected wall time)

Worst-case wait, --max-retries 30:
  15.5 + 25 × 30 ≈ 770 seconds per stuck block
  → 770 / 44 ≈ 17× expected wall time
```

A *single* block that exhausts the default 5 retries adds **35% to the
entire expected wall time** of a 1 TiB transfer. Going to 10 retries
already extends one stuck block to **~2.5 minutes** — more than 3× the
whole transfer's expected time. Going to 30 retries can push one stuck
block past **~13 minutes**, 17× the expected transfer time.

This is why `--max-retries` is the *third* lever, not the first. Used
alone it can turn a 44-second job into a multi-minute one. Used after
`--download-ranks` has already removed the underlying overload, it's the
right safety net for transient bursts.

---

## How backoff works

Source: `src/storage/blob/client.rs`. Three rules:

### 1. What gets retried

| Status / Error | Retried? |
|---|---|
| `503 ServerBusy` | ✅ |
| `429 TooManyRequests` | ✅ |
| `500`, `502`, `504` | ✅ |
| Transport: connect / timeout / body-streaming / request-build | ✅ |
| `4xx` auth/config (`401`, `403`, `404`, …) | ❌ — fails fast |

### 2. How long each wait is

The first rule wins:

**a. `Retry-After` header (server hint).** If Azure's response includes
`Retry-After: <seconds>`, that wait is honored exactly, capped at 60 s.
Azure Blob does set this on `503` in practice (typically 1-30 s), so
under heavy throttling the schedule is effectively driven by the server,
not the client.

**b. Exponential backoff with ±25% jitter** (when no `Retry-After`):

```
wait_ms = min(500 * 2^(attempt-1), 30_000) ± 25% jitter
```

| Attempt | Base wait | Range with jitter |
|---|---|---|
| 1 → 2 | 500 ms | 375 ms – 625 ms |
| 2 → 3 | 1.0 s | 750 ms – 1.25 s |
| 3 → 4 | 2.0 s | 1.5 s – 2.5 s |
| 4 → 5 | 4.0 s | 3.0 s – 5.0 s |
| 5 → 6 | 8.0 s | 6.0 s – 10.0 s |
| 6 → 7 | 16.0 s | 12.0 s – 20.0 s |
| 7 → 8 | 30.0 s (cap) | 22.5 s – 37.5 s |
| 8+ | 30.0 s (cap) | 22.5 s – 37.5 s |

So `--max-retries 5` (default) gives ~15.5 s worst case per block;
`--max-retries 10` gives ~2.5 minutes; `--max-retries 15` gives ~5
minutes; `--max-retries 30` gives ~13 minutes. See the cost analysis in
Knob 3.

### 3. Live retry counters

When progress display is active, every downloader's progress bar suffix
shows that rank's live counters, e.g.

```
[retry 503x12 429x2]
```

If those numbers are climbing during a run, **stop the job and lower
`--download-ranks` or `--max-bandwidth`.** Don't just let retries soak
it up — see the cost math above.

In Slurm / CI / `kubectl logs` runs where stderr isn't a TTY the live
bars are silenced automatically. For those, `azcp-cluster` prints a
single MPI-reduced summary line on rank 0 at end of the download stage:

```
[cluster] retries: 503x842 429x14 across 16 downloaders
```

The line is omitted when no rank saw any retry, so its presence alone
means Azure pushed back during the run. Categories shown only when
non-zero: `503xN` (ServerBusy), `429xN` (Too Many Requests), `5xxxN`
(other transient 5xx), `transport-errxN` (connection / timeout).

If a transfer ultimately fails after exhausting retries, `azcp-cluster`
exits non-zero and prints a `Failed: N` summary.

---

## Recipe

Anchored on the v0.4.2 throttling sweep: 16 × ND H100 v5 nodes
against a single same-region standard account, where the 200 Gb/s
ingress soft limit is a **hard cliff** (177 Gb/s @ `--concurrency 32`
→ 0 retries; 262 Gb/s @ `--concurrency 64` → 3 347 retries). Capping
bandwidth at the threshold eliminates all retries with **no
wall-clock penalty**. Full data:
[docs/cluster-v0.4.2-throttling-sweep.md](cluster-v0.4.2-throttling-sweep.md).

```bash
srun --mpi=pmix azcp-cluster download <src> <dst> \
  --shard-size 2GiB \
  --concurrency 64 \
  --max-bandwidth 200Gbps \
  --max-retries 15 \
  --block-size 16MiB \
  --bcast-chunk 1GiB \
  --bcast-pipeline 128 \
  --bcast-writers 8 \
  --no-progress
```

The first four flags are the throttling controls. The last five are
the bcast / progress tuning carried over unchanged from the v0.3.x
1.2 TB / 6-file sweep
([docs/azcp-cluster-1tb-6file-sweep.md](azcp-cluster-1tb-6file-sweep.md))
where they were established as the few-large-files winners
(`--block-size 16MiB`, `--bcast-chunk 1GiB`, `--bcast-pipeline 128`,
`--bcast-writers 8`). Use the same scaffolding `srun` flags as in
that sweep (UCX env, `--mem-bind=local`, `taskset -c 0-47` NUMA pin,
container mounts) — full block in
[docs/cluster-v0.4.2-throttling-sweep.md § Constant configuration](cluster-v0.4.2-throttling-sweep.md#constant-configuration).

`--shard-size 2GiB` ensures all 16 downloaders have work even when the
dataset has few large files (see Knob 2 caveat). `--concurrency 64`
sizes per-rank in-flight requests for the worst case; `--max-bandwidth
200Gbps` protects the storage account from 503s without slowing the
run down.

If `--max-bandwidth` is unavailable (older client), fall back to
`--shard-size 2GiB --concurrency 32` — same 0 retries, ~10 s slower
on a 1.2 TB workload.

Scale `--download-ranks` and `--max-bandwidth` together if you have a
higher account quota and VMs with sufficient NIC bandwidth — the
relationship between K and quota is roughly linear in the regime we've
measured. If you're not sure, start at K = 16 and watch the retry
counters (live in the progress bar, or the rank-0 `[cluster] retries:`
summary in Slurm/CI runs).

---

## Still seeing 503s?

If you've sized `--download-ranks` and `--max-bandwidth` to your account
quota and **still** see throttling:

1. **Request a quota increase.** Standard accounts can be raised
   well above the 200 Gbps default via Azure support. This is the
   straightforward fix when one workload genuinely needs more egress.
2. **Split across multiple accounts.** Each account has independent
   throttling. For very large datasets (multi-TiB) or very large
   clusters (1000+ nodes), one account is the wrong shape.
3. **Cohabiting workload.** Another job on the same account may be
   eating quota. Coordinate, or move workloads apart.
4. **Cool / Archive tier.** These tiers have lower egress ceilings.
   Check `x-ms-access-tier` on the blobs; rehydrate to Hot before bulk
   downloads.
5. **Region mismatch.** Account region ≠ compute region adds latency
   and reduces effective throughput. Co-locate.
