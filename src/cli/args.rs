use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

// Resolve the three-state progress preference: explicit --no-progress wins,
// then explicit --progress, otherwise auto-detect based on whether stderr is
// attached to a terminal. Auto-detection avoids polluting redirected logs and
// CI captures with cursor-rewriting escapes; the explicit flags are escape
// hatches for when detection guesses wrong (e.g. allocated PTY in CI, or
// nohup'd run being tail -f'd).
pub fn resolve_progress(progress: bool, no_progress: bool) -> bool {
    use std::io::IsTerminal;
    if no_progress {
        return false;
    }
    if progress {
        return true;
    }
    std::io::stderr().is_terminal()
}

// Parse a human-readable bandwidth string into bytes/sec.
//
// Accepts both bit-rate and byte-rate units, decimal (powers of 1000) and
// binary (powers of 1024), with or without a "/s" suffix. Examples:
//   50Gbps, 200Mbps, 1Gbps          (bits per second, decimal)
//   1GB/s, 500MB/s, 100KB/s         (bytes per second, decimal)
//   500MiB/s, 1GiB/s                (bytes per second, binary)
//   8000000                         (bare = bytes per second)
pub fn parse_bandwidth(s: &str) -> Result<u64, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("empty bandwidth value".into());
    }
    let lower = raw.to_lowercase();
    let body = lower.strip_suffix("/s").unwrap_or(&lower);

    let split = body
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(body.len());
    let (num_str, unit) = body.split_at(split);
    if num_str.is_empty() {
        return Err(format!(
            "bandwidth must start with a number, got {s:?} \
             (try e.g. 50Gbps, 1GB/s, 500MiB/s)"
        ));
    }
    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("bad bandwidth number {num_str:?} in {s:?}"))?;
    if num < 0.0 || !num.is_finite() {
        return Err(format!("bandwidth must be positive and finite, got {s:?}"));
    }

    let multiplier: f64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "tb" => 1_000_000_000_000.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "bps" => 1.0 / 8.0,
        "kbps" => 1_000.0 / 8.0,
        "mbps" => 1_000_000.0 / 8.0,
        "gbps" => 1_000_000_000.0 / 8.0,
        "tbps" => 1_000_000_000_000.0 / 8.0,
        other => {
            return Err(format!(
                "unknown bandwidth unit {other:?} in {s:?} \
                 (try Gbps, Mbps, GB/s, MiB/s, GiB/s, ...)"
            ))
        }
    };

    let bytes_per_sec = (num * multiplier).round();
    if bytes_per_sec < 1.0 {
        return Err(format!("bandwidth {s:?} rounds to 0 bytes/sec"));
    }
    Ok(bytes_per_sec as u64)
}

pub fn parse_shard(s: &str) -> Result<(usize, usize), String> {
    let (i, n) = s
        .split_once('/')
        .ok_or_else(|| format!("expected INDEX/COUNT, got {s:?}"))?;
    let i: usize = i.parse().map_err(|_| format!("bad shard index {i:?}"))?;
    let n: usize = n.parse().map_err(|_| format!("bad shard count {n:?}"))?;
    if n == 0 {
        return Err("shard count must be >= 1".into());
    }
    if i >= n {
        return Err(format!("shard index {i} must be < count {n}"));
    }
    Ok((i, n))
}

#[derive(Parser)]
#[command(
    name = "azcp",
    version,
    about = "Azure Storage data transfer tool (Rust)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[arg(long, global = true, default_value = "info")]
    pub log_level: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Copy data between local filesystem and Azure Blob Storage
    Copy(CopyArgs),

    /// Synchronize source and destination, transferring only changed files
    Sync(SyncArgs),

    /// List containers or blobs
    #[command(name = "ls", alias = "list")]
    List(ListArgs),

    /// Remove blobs or containers
    #[command(name = "rm", alias = "remove")]
    Remove(RemoveArgs),

    /// Create a container or local directory
    #[command(name = "mk", alias = "make")]
    Make(MakeArgs),

    /// Show environment variable configuration
    Env,
}

#[derive(Args, Clone)]
pub struct CopyArgs {
    pub source: String,
    pub destination: String,

    #[arg(long, short)]
    pub recursive: bool,

    #[arg(long, help = "Do not overwrite existing destination files")]
    pub no_overwrite: bool,

    #[arg(long, default_value_t = 4_194_304)]
    pub block_size: u64,

    #[arg(long, default_value_t = 64)]
    pub concurrency: usize,

    #[arg(
        long,
        env = "AZCP_PARALLEL_FILES",
        default_value_t = 16,
        help = "Max files actively transferring concurrently (chunks within each share --concurrency budget)"
    )]
    pub parallel_files: usize,

    #[arg(
        long,
        env = "AZCP_MAX_RETRIES",
        default_value_t = 5,
        help = "Max retry attempts for throttled (503/429) and transient 5xx responses"
    )]
    pub max_retries: u32,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(
        long,
        help = "Download bytes from network but discard them (no disk writes). Benchmarks pure network throughput. Download-only."
    )]
    pub discard: bool,

    #[arg(long)]
    pub check_md5: bool,

    #[arg(long)]
    pub include_pattern: Option<String>,

    #[arg(long)]
    pub exclude_pattern: Option<String>,

    #[arg(
        long,
        conflicts_with = "no_progress",
        help = "Force per-file progress bars on (overrides TTY auto-detect; default: on if stderr is a TTY)"
    )]
    pub progress: bool,

    #[arg(long, help = "Disable progress display (overrides TTY auto-detect)")]
    pub no_progress: bool,

    #[arg(
        long,
        value_parser = parse_shard,
        help = "Process only this shard of the workload (INDEX/COUNT, e.g. 0/8). Run N invocations with different indices for multi-process throughput."
    )]
    pub shard: Option<(usize, usize)>,

    #[arg(
        long,
        default_value_t = 1,
        help = "Spawn N independent tokio runtimes (each with its own connection pool + shard). Single-process alternative to external --shard."
    )]
    pub workers: usize,

    #[arg(
        long,
        value_name = "FILE",
        help = "Read source blob listing from FILE (TSV: <name>\\t<size>[\\t<modified>]) instead of calling LIST. Generate with: azcp ls <url> --recursive --machine-readable > FILE. Download-only."
    )]
    pub shardlist: Option<PathBuf>,

    #[arg(
        long,
        value_name = "RATE",
        value_parser = parse_bandwidth,
        help = "Cap aggregate throughput across all workers. Accepts bit-rate (e.g. 50Gbps, 200Mbps) or byte-rate (e.g. 1GB/s, 500MiB/s, 100KB/s). Applies to both uploads and downloads."
    )]
    pub max_bandwidth: Option<u64>,
}

#[derive(Args)]
pub struct SyncArgs {
    pub source: String,
    pub destination: String,

    #[arg(long, default_value_t = 64)]
    pub concurrency: usize,

    #[arg(long, default_value_t = 4_194_304)]
    pub block_size: u64,

    #[arg(
        long,
        env = "AZCP_PARALLEL_FILES",
        default_value_t = 16,
        help = "Max files actively transferring concurrently"
    )]
    pub parallel_files: usize,

    #[arg(
        long,
        env = "AZCP_MAX_RETRIES",
        default_value_t = 5,
        help = "Max retry attempts for throttled (503/429) and transient 5xx responses"
    )]
    pub max_retries: u32,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, help = "Delete destination files not present in source")]
    pub delete_destination: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = CompareMethod::SizeAndMtime,
        help = "How to decide whether a file needs re-transfer"
    )]
    pub compare_method: CompareMethod,

    #[arg(long)]
    pub include_pattern: Option<String>,

    #[arg(long)]
    pub exclude_pattern: Option<String>,

    #[arg(
        long,
        conflicts_with = "no_progress",
        help = "Force per-file progress bars on (overrides TTY auto-detect; default: on if stderr is a TTY)"
    )]
    pub progress: bool,

    #[arg(long, help = "Disable progress display (overrides TTY auto-detect)")]
    pub no_progress: bool,

    #[arg(
        long,
        value_parser = parse_shard,
        help = "Process only this shard of the workload (INDEX/COUNT, e.g. 0/8)."
    )]
    pub shard: Option<(usize, usize)>,
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum CompareMethod {
    /// Re-transfer if file sizes differ.
    Size,
    /// Re-transfer if size differs OR source last-modified > destination last-modified.
    SizeAndMtime,
    /// Re-transfer if local MD5 differs from blob's stored content-md5.
    Md5,
    /// Re-transfer everything.
    Always,
}

#[derive(Args)]
pub struct ListArgs {
    pub url: String,

    #[arg(long, short)]
    pub recursive: bool,

    #[arg(long)]
    pub machine_readable: bool,
}

#[derive(Args)]
pub struct RemoveArgs {
    pub url: String,

    #[arg(long, short)]
    pub recursive: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, default_value_t = 64)]
    pub concurrency: usize,

    #[arg(
        long,
        env = "AZCP_MAX_RETRIES",
        default_value_t = 5,
        help = "Max retry attempts for throttled (503/429) and transient 5xx responses"
    )]
    pub max_retries: u32,

    #[arg(long)]
    pub include_pattern: Option<String>,

    #[arg(long)]
    pub exclude_pattern: Option<String>,

    #[arg(
        long,
        conflicts_with = "no_progress",
        help = "Force progress display on (overrides TTY auto-detect; default: on if stderr is a TTY)"
    )]
    pub progress: bool,

    #[arg(long, help = "Disable progress display (overrides TTY auto-detect)")]
    pub no_progress: bool,
}

#[derive(Args)]
pub struct MakeArgs {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_bit_units() {
        assert_eq!(parse_bandwidth("8bps").unwrap(), 1);
        assert_eq!(parse_bandwidth("1Kbps").unwrap(), 125);
        assert_eq!(parse_bandwidth("1Mbps").unwrap(), 125_000);
        assert_eq!(parse_bandwidth("1Gbps").unwrap(), 125_000_000);
        assert_eq!(parse_bandwidth("50Gbps").unwrap(), 6_250_000_000);
        assert_eq!(parse_bandwidth("200Mbps").unwrap(), 25_000_000);
    }

    #[test]
    fn bandwidth_byte_units_decimal() {
        assert_eq!(parse_bandwidth("1KB/s").unwrap(), 1_000);
        assert_eq!(parse_bandwidth("1MB/s").unwrap(), 1_000_000);
        assert_eq!(parse_bandwidth("1GB/s").unwrap(), 1_000_000_000);
        assert_eq!(parse_bandwidth("500MB/s").unwrap(), 500_000_000);
    }

    #[test]
    fn bandwidth_byte_units_binary() {
        assert_eq!(parse_bandwidth("1KiB/s").unwrap(), 1024);
        assert_eq!(parse_bandwidth("1MiB/s").unwrap(), 1024 * 1024);
        assert_eq!(parse_bandwidth("500MiB/s").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_bandwidth("1GiB/s").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn bandwidth_bare_bytes() {
        assert_eq!(parse_bandwidth("12345").unwrap(), 12345);
        assert_eq!(parse_bandwidth("1000000").unwrap(), 1_000_000);
    }

    #[test]
    fn bandwidth_case_insensitive() {
        assert_eq!(parse_bandwidth("50gbps").unwrap(), 6_250_000_000);
        assert_eq!(parse_bandwidth("1gib/s").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn bandwidth_rejects_garbage() {
        assert!(parse_bandwidth("").is_err());
        assert!(parse_bandwidth("nope").is_err());
        assert!(parse_bandwidth("100xyz").is_err());
        assert!(parse_bandwidth("-5Gbps").is_err());
        assert!(parse_bandwidth("0.0001bps").is_err());
    }
}
