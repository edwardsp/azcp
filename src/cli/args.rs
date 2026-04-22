use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

    #[arg(long, help = "Show per-file progress bars with throughput")]
    pub progress: bool,

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

    #[arg(long, help = "Show per-file progress bars with throughput")]
    pub progress: bool,

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

    #[arg(long, help = "Show progress bar with deletion rate")]
    pub progress: bool,
}

#[derive(Args)]
pub struct MakeArgs {
    pub url: String,
}
