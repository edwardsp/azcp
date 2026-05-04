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

    /// Maximum retries per HTTP request
    #[arg(long, default_value_t = 5)]
    pub max_retries: u32,

    /// Disable per-rank progress bars (default: TTY-aware)
    #[arg(long)]
    pub no_progress: bool,

    /// Run only the named stage and exit. Hidden debug knob used by per-task
    /// QA scenarios.
    #[arg(long, value_enum, default_value_t = Stage::All, hide = true)]
    pub stage: Stage,
}
