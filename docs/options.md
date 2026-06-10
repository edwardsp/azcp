# CLI options reference

Every command-line option across the two binaries — `azcp` (the single-binary
CLI) and `azcp-cluster` (the companion MPI broadcast tool). For prose,
examples, and tuning guidance see the [README](../README.md),
[docs/performance-tuning.md](performance-tuning.md), and
[docs/cluster.md](cluster.md).

**Legend**

| Mark | Meaning |
|:---:|---|
| ✅ | Supported as a flag |
| ➖ | No flag, but the behaviour is **always on** |
| ❌ | Not applicable / no equivalent |

**Reading the `azcp` column.** `azcp` is a multi-command CLI (`copy`, `sync`,
`ls`, `rm`, `mk`, `env`); this table is primarily about **`azcp copy`**, which
carries the bulk of the options. Where an option belongs to a *different*
subcommand, the description says so (e.g. *`sync` only*). Positional arguments
(`source`, `destination`, `url`) are omitted — only flags are listed.

Environment variables are not given their own column; the few that back an
option are noted inline in the description, and `azcp env` prints every
variable `azcp` recognises (auth, logging, concurrency).

## Options

| Option | azcp | azcp-cluster | Description |
|---|:---:|:---:|---|
| `--block-size <SIZE>` | ✅ | ✅ | Upload/download block size. Accepts bytes or binary/decimal units (`16MiB`, `16MB`). Default **4 MiB** (`azcp`), **16 MiB** (`azcp-cluster`). `azcp`: `copy`/`sync`. |
| `--concurrency <N>` | ✅ | ✅ | Max in-flight HTTP block requests (per worker on `azcp`, per rank on `azcp-cluster`). Default **64**. `azcp`: `copy`/`sync`/`rm`. |
| `--parallel-files <N>` | ✅ | ✅ | Max files transferring concurrently. Default **16**. Env `AZCP_PARALLEL_FILES` (`azcp` `copy`/`sync`). |
| `--max-retries <N>` | ✅ | ✅ | Retry budget for throttled (`503`/`429`) and transient `5xx` responses, with exponential backoff + jitter. Default **5**. Env `AZCP_MAX_RETRIES` (`azcp` `copy`/`sync`/`rm`). |
| `--max-bandwidth <RATE>` | ✅ | ✅ | Cap aggregate throughput. Bit-rate (`50Gbps`, `200Mbps`) or byte-rate (`1GB/s`, `500MiB/s`, `100KB/s`); trailing `/s` optional. `azcp`: `copy` only. `azcp-cluster`: the cap is divided across the downloader ranks. |
| `--shardlist <FILE>` | ✅ | ✅ | Read the blob listing from a TSV file (`<name>\t<size>[\t<modified>]`) instead of calling LIST. Generate with `azcp ls <url> --recursive --machine-readable`. **Download-only.** `azcp`: `copy`. |
| `--no-progress` | ✅ | ✅ | Disable the progress display (overrides TTY auto-detect). `azcp`: `copy`/`sync`/`rm`. |
| `--recursive`, `-r` | ✅ | ➖ | Recurse into the source prefix. `azcp`: `copy`/`ls`/`rm`. **`azcp-cluster` is always recursive** — no flag; it always treats the source as a prefix/dataset. |
| `--workers <N>` | ✅ | ❌ | Spawn N independent tokio runtimes in one process, each with its own connection pool + shard, to scale past the ~28 Gbps single-runtime ceiling. Default **1**. `azcp` `copy` only. **N/A on `azcp-cluster`** — it scales by node count (one MPI rank per node), not by an intra-process worker count. |
| `--shard <INDEX/COUNT>` | ✅ | ❌ | Process only shard *INDEX* of *COUNT* (0-based) for multi-process throughput; the partition is deterministic so N invocations cooperate without coordination. `azcp`: `copy`/`sync`. (`azcp-cluster` shards the workload across ranks internally.) |
| `--no-overwrite` | ✅ | ❌ | Skip destination files that already exist. `azcp` `copy` only. (`azcp-cluster` always overwrites.) |
| `--check-md5` | ✅ | ❌ | Store a `Content-MD5` on upload so a later `sync --compare-method md5` has something to compare against. `azcp` `copy` only. (Cluster verifies with `--verify`.) |
| `--dry-run` | ✅ | ❌ | List the planned actions without transferring anything. `azcp`: `copy`/`sync`/`rm`. |
| `--discard` | ✅ | ❌ | Download bytes from the network but discard them (no disk writes) to benchmark pure network throughput. **Download-only.** `azcp` `copy` only. |
| `--direct` | ✅ | ❌ | Open destination files with `O_DIRECT` (Linux/macOS), bypassing the page cache; aligned writes with an `ftruncate` tail fix-up. **Download-only.** `azcp` `copy` only. (Cluster has `--no-bcast-direct` for its bcast write path.) |
| `--include-pattern <GLOB>` | ✅ | ❌ | Only transfer blobs/files matching the glob. `azcp`: `copy`/`sync`/`rm`. |
| `--exclude-pattern <GLOB>` | ✅ | ❌ | Skip blobs/files matching the glob. `azcp`: `copy`/`sync`/`rm`. |
| `--progress` | ✅ | ❌ | Force the progress display on (overrides TTY auto-detect; default: on when stderr is a TTY). `azcp`: `copy`/`sync`/`rm`. (Cluster only exposes `--no-progress`.) |
| `--compare-method <METHOD>` | ✅ | ❌ | `sync` diff strategy: `size`, `size-and-mtime` (default), `md5`, `always`. **`azcp sync` only.** (Cluster's equivalent is `--compare`.) |
| `--delete-destination` | ✅ | ❌ | Delete destination files not present in the source. **`azcp sync` only.** |
| `--machine-readable` | ✅ | ❌ | Emit TSV (`<name>\t<size>\t<modified>`) suitable for `--shardlist`. **`azcp ls` only.** |
| `--log-level <LEVEL>` | ✅ | ❌ | Global log verbosity (default `info`). `azcp` global flag (applies to every subcommand). |
| `--compare <POLICY>` | ❌ | ✅ | What to re-transfer: `none` (default), `size`, or `filelist`. |
| `--filelist <FILE>` | ❌ | ✅ | Prior-run filelist (sizes + `Last-Modified`); **required** with `--compare filelist`. |
| `--save-filelist <FILE>` | ❌ | ✅ | After the run, rank 0 writes the post-run filelist to `FILE` (feed it back via `--filelist` next time). |
| `--bcast-chunk <SIZE>` | ❌ | ✅ | `MPI_Bcast` chunk size. Accepts size units (`1GiB`, `64MiB`). Default **64 MiB**. |
| `--bcast-pipeline <N>` | ❌ | ✅ | Number of in-flight `Ibcast` chunks (pipeline depth); each costs `bcast_pipeline × bcast_chunk` bytes/rank. Default **1** (best on TCP-only fabrics; raise for RDMA/multi-rail). |
| `--bcast-writers <N>` | ❌ | ✅ | Writer threads on receiver ranks for bcast output (dispatched by `file_id % N`). Default **2**. |
| `--no-bcast-direct` | ❌ | ✅ | Disable `O_DIRECT` for bcast output files (use on NFS / some FUSE mounts that don't support it). By default O_DIRECT is on. |
| `--download-ranks <K>` | ❌ | ✅ | Number of ranks that actually download from Azure (default: all ranks). The rest skip the download and only participate in the broadcast — useful when too many concurrent downloaders trigger `503` throttling. |
| `--verify` | ❌ | ✅ | After bcast, compute MD5 of every local file on every rank and cross-check (also against the blob's `Content-MD5` where present). Mismatch exits non-zero. |
| `--shard-size <SIZE>` | ❌ | ✅ | Range-shard each file into byte chunks of this size before distributing across ranks. Default **0** (one shard per file). When `>0` must be a multiple of `--bcast-chunk`. Mutually exclusive with `--file-shards`. |
| `--file-shards <N>` | ❌ | ✅ | Convenience knob: derive `--shard-size` as `ceil(max_file_size / N)` rounded up to a `--bcast-chunk` multiple. Mutually exclusive with `--shard-size`. |
| `--stage <STAGE>` | ❌ | ✅ † | **Hidden debug flag.** Run only one stage and exit: `all` (default), `init`, `list`, `download`, `bcast`. Used by QA scenarios; not shown in `--help`. |

† **Hidden flags** are functional but omitted from `--help` output. `--stage`
is the only one today (`azcp-cluster`).

## Source of truth

These tables are maintained by hand from the argument definitions; if you add
or change a flag, update this doc too.

- `azcp` — [`src/cli/args.rs`](../src/cli/args.rs): global `--log-level` plus
  `CopyArgs`, `SyncArgs`, `ListArgs`, `RemoveArgs`, `MakeArgs`.
- `azcp-cluster` — [`crates/azcp-cluster/src/cli.rs`](../crates/azcp-cluster/src/cli.rs): `Args`.

Run `azcp <command> --help` or `azcp-cluster --help` for the authoritative,
version-specific list.
