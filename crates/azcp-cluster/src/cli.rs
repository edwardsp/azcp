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
}
