use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Compare {
    None,
    Size,
    Filelist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Stage {
    All,
    Init,
    List,
    Download,
    Bcast,
}

#[derive(Debug, Parser)]
#[command(
    name = "azcp-cluster",
    about = "MPI cluster broadcast copy: shard download from Azure Blob, then \
             MPI_Bcast across the cluster so every node has a local copy.",
    version
)]
pub struct Args {
    /// Source blob URL, e.g. https://acct.blob.core.windows.net/ctr/prefix/
    pub source: String,

    /// Destination directory on every node
    pub dest: PathBuf,

    /// Skip the LIST API and read the file list from FILE (TSV)
    #[arg(long, value_name = "FILE")]
    pub shardlist: Option<PathBuf>,

    /// Compare policy controlling what gets re-transferred
    #[arg(long, value_enum, default_value_t = Compare::None)]
    pub compare: Compare,

    /// Required when --compare filelist: prior-run filelist with sizes + Last-Modified
    #[arg(long, value_name = "FILE")]
    pub filelist: Option<PathBuf>,

    /// After the run, rank 0 writes the post-run filelist to FILE
    #[arg(long, value_name = "FILE")]
    pub save_filelist: Option<PathBuf>,

    /// In-flight HTTP block requests per rank (forwarded to azcp engine)
    #[arg(long, default_value_t = 64)]
    pub concurrency: usize,

    /// Block size in bytes for downloads
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub block_size: usize,

    /// Files transferring concurrently per rank
    #[arg(long, default_value_t = 16)]
    pub parallel_files: usize,

    /// Bcast chunk size in bytes
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    pub bcast_chunk: usize,

    /// Number of in-flight Ibcast chunks (depth of the pipeline). Higher
    /// values overlap disk I/O with the network at the cost of memory
    /// (`bcast_pipeline * bcast_chunk` bytes per rank). 1 disables
    /// pipelining and is currently the best default on TCP-only fabrics
    /// where OpenMPI's MPI_Bcast already pipelines internally; raise it
    /// when you have RDMA or multi-rail links where additional
    /// concurrency actually finds more bandwidth.
    #[arg(long, default_value_t = 1)]
    pub bcast_pipeline: usize,

    /// Number of writer threads on receiver ranks for bcast output.
    /// Single-threaded buffered writes can bottleneck below NVMe line
    /// rate; fanning across multiple threads (dispatched by file_id %
    /// N) restores throughput. Owner ranks are unaffected.
    #[arg(long, default_value_t = 2)]
    pub bcast_writers: usize,

    /// Disable O_DIRECT for bcast output files. By default, output files
    /// are opened with O_DIRECT to bypass the page cache, which on fast
    /// NVMe lifts single-thread write throughput from ~47 Gb/s to
    /// ~100 Gb/s. The trailing partial chunk of each file is rounded up
    /// to alignment and ftruncate'd back at close. Use this flag on
    /// filesystems that don't support O_DIRECT (NFS, some FUSE mounts).
    #[arg(long)]
    pub no_bcast_direct: bool,

    /// Maximum retries per HTTP request
    #[arg(long, default_value_t = 5)]
    pub max_retries: u32,

    /// Cap aggregate cluster download throughput. Accepts bit-rate
    /// (50Gbps, 200Mbps) or byte-rate (1GB/s, 500MiB/s, 100KB/s).
    /// The cap is divided across the active downloader ranks (see
    /// --download-ranks); each rank gets total / K.
    #[arg(long, value_name = "RATE", value_parser = azcp::cli::args::parse_bandwidth)]
    pub max_bandwidth: Option<u64>,

    /// Number of ranks that actually download from Azure (default: all
    /// ranks). The remaining ranks skip the download phase and only
    /// participate in MPI_Bcast. Useful when too many concurrent
    /// downloaders against one storage account cause 503 throttling.
    #[arg(long, value_name = "K")]
    pub download_ranks: Option<usize>,

    /// After bcast, compute MD5 of every local file on every rank and
    /// cross-check. Where the blob has a Content-MD5 (typically only
    /// small single-shot uploads), also verify against that. Mismatch
    /// is logged and exits non-zero.
    #[arg(long)]
    pub verify: bool,

    /// Disable per-rank progress bars (default: TTY-aware)
    #[arg(long)]
    pub no_progress: bool,

    /// Run only the named stage and exit. Hidden debug knob used by per-task
    /// QA scenarios.
    #[arg(long, value_enum, default_value_t = Stage::All, hide = true)]
    pub stage: Stage,

    /// Range-shard each file into byte chunks of this size before
    /// distributing across ranks. 0 (default) = no sharding (one shard
    /// per file, identical to v0.3.1 plan shape). When >0, must be a
    /// multiple of --bcast-chunk so per-shard O_DIRECT padding never
    /// straddles shard boundaries. Accepts byte sizes with units:
    /// 32GiB, 1GB, 16777216. Mutually exclusive with --file-shards.
    #[arg(long, default_value_t = 0, value_parser = parse_size)]
    pub shard_size: u64,

    /// Convenience knob: derive --shard-size as
    /// `ceil(max_file_size / N)` rounded up to a multiple of
    /// --bcast-chunk. Mutually exclusive with --shard-size.
    #[arg(long, value_name = "N")]
    pub file_shards: Option<usize>,
}

/// Parse a byte size with optional binary/decimal SI suffix.
/// Accepts: bare bytes (`16777216`), decimal (`32MB`, `1GB`, `2TB`),
/// binary (`32MiB`, `1GiB`, `2TiB`). Case-insensitive. The trailing
/// `B` is optional (`32Gi`, `32G` both work as binary GiB / decimal GB
/// respectively — same convention as `dd`).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("empty size value".into());
    }
    let lower = raw.to_lowercase();
    let split = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (num_str, unit) = lower.split_at(split);
    if num_str.is_empty() {
        return Err(format!(
            "size must start with a number, got {s:?} (try e.g. 32GiB, 1GB, 16777216)"
        ));
    }
    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("bad size number {num_str:?} in {s:?}"))?;
    if num < 0.0 || !num.is_finite() {
        return Err(format!("size must be non-negative and finite, got {s:?}"));
    }
    let multiplier: f64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(format!(
                "unknown size unit {other:?} in {s:?} (try GiB, MiB, GB, MB)"
            ))
        }
    };
    Ok((num * multiplier).round() as u64)
}

impl Args {
    /// Validate cross-argument invariants that clap can't express
    /// declaratively. Call once after `Args::parse()`.
    pub fn validate(&self) -> Result<(), String> {
        if self.shard_size > 0 && self.file_shards.is_some() {
            return Err(
                "--shard-size and --file-shards are mutually exclusive; pass at most one".into(),
            );
        }
        if self.shard_size > 0 {
            let chunk = self.bcast_chunk as u64;
            if chunk == 0 {
                return Err("--bcast-chunk must be > 0".into());
            }
            if self.shard_size % chunk != 0 {
                return Err(format!(
                    "--shard-size ({}) must be a multiple of --bcast-chunk ({}) so per-shard \
                     O_DIRECT padding never straddles shard boundaries",
                    self.shard_size, chunk
                ));
            }
        }
        if let Some(n) = self.file_shards {
            if n == 0 {
                return Err("--file-shards must be >= 1".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("16777216").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("32GiB").unwrap(), 32u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("32gib").unwrap(), 32u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("32Gi").unwrap(), 32u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("2TiB").unwrap(), 2u64 * 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("12XYZ").is_err());
        assert!(parse_size("-5").is_err());
    }

    fn args_with(shard_size: u64, file_shards: Option<usize>, bcast_chunk: usize) -> Args {
        Args {
            source: String::new(),
            dest: PathBuf::new(),
            shardlist: None,
            compare: Compare::None,
            filelist: None,
            save_filelist: None,
            concurrency: 64,
            block_size: 16 * 1024 * 1024,
            parallel_files: 16,
            bcast_chunk,
            bcast_pipeline: 1,
            bcast_writers: 2,
            no_bcast_direct: false,
            max_retries: 5,
            max_bandwidth: None,
            download_ranks: None,
            verify: false,
            no_progress: false,
            stage: Stage::All,
            shard_size,
            file_shards,
        }
    }

    #[test]
    fn validate_default_off() {
        assert!(args_with(0, None, 64 * 1024 * 1024).validate().is_ok());
    }

    #[test]
    fn validate_mutex_rejected() {
        let err = args_with(1 << 30, Some(8), 64 * 1024 * 1024)
            .validate()
            .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn validate_shard_must_be_chunk_multiple() {
        let err = args_with(1_000_000, None, 64 * 1024 * 1024)
            .validate()
            .unwrap_err();
        assert!(err.contains("multiple"), "got: {err}");
    }

    #[test]
    fn validate_shard_aligned_ok() {
        assert!(
            args_with(64 * 1024 * 1024, None, 64 * 1024 * 1024)
                .validate()
                .is_ok()
        );
        assert!(
            args_with(2u64 * 1024 * 1024 * 1024, None, 64 * 1024 * 1024)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn validate_file_shards_zero_rejected() {
        let err = args_with(0, Some(0), 64 * 1024 * 1024)
            .validate()
            .unwrap_err();
        assert!(err.contains(">= 1"), "got: {err}");
    }
}
