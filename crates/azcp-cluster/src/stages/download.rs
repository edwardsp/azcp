use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use azcp::{
    parse_location, BlobClient, Credential, DownloadRange, Location, TransferConfig, TransferEngine,
};

use crate::cli::Args;

/// Per-rank counters returned to the orchestrator so rank 0 can MPI_Reduce
/// them into a cluster-wide retry summary. Mirrors the four atomics in
/// `azcp::storage::blob::client::RetryStats` so we don't leak that type
/// across the crate boundary.
#[derive(Debug, Default, Clone, Copy)]
pub struct RankRetrySummary {
    pub throttle_503: u64,
    pub throttle_429: u64,
    pub server_5xx: u64,
    pub transport_err: u64,
}

pub fn run(
    args: &Args,
    my_ranges: Vec<DownloadRange>,
    max_bandwidth: Option<u64>,
) -> Result<RankRetrySummary> {
    if my_ranges.is_empty() {
        return Ok(RankRetrySummary::default());
    }
    let loc = parse_location(&args.source)
        .map_err(|e| anyhow!("failed to parse source {}: {e}", args.source))?;
    let blob = match loc {
        Location::AzureBlob(b) => b,
        Location::Local(p) => {
            return Err(anyhow!(
                "source must be an Azure Blob URL (got local path {p})"
            ))
        }
    };

    let credential = Credential::resolve_or_anonymous(&blob.account, blob.sas_token.as_deref())
        .map_err(|e| anyhow!("failed to resolve credentials for {}: {e}", blob.account))?;
    let client =
        BlobClient::new(credential).map_err(|e| anyhow!("failed to construct BlobClient: {e}"))?;
    let retry_stats = client.retry_stats();

    let config = TransferConfig {
        block_size: args.block_size as u64,
        concurrency: args.concurrency,
        parallel_files: args.parallel_files,
        dry_run: false,
        discard: false,
        overwrite: true,
        recursive: true,
        include_pattern: None,
        exclude_pattern: None,
        check_md5: false,
        progress: !args.no_progress,
        max_retries: args.max_retries,
        shard: None,
        shardlist: None,
        max_bandwidth_bytes_per_sec: max_bandwidth,
        direct: false,
    };

    let engine = TransferEngine::new(client, config)
        .map_err(|e| anyhow!("failed to build TransferEngine: {e}"))?;
    let engine = Arc::new(engine);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for download")?;

    let summary = rt
        .block_on(engine.download_ranges(my_ranges, &blob.account, &blob.container))
        .map_err(|e| anyhow!("download failed: {e}"))?;

    if summary.failed > 0 {
        return Err(anyhow!(
            "download stage: {} of {} files failed",
            summary.failed,
            summary.total_files
        ));
    }
    Ok(RankRetrySummary {
        throttle_503: retry_stats.throttle_503.load(Ordering::Relaxed),
        throttle_429: retry_stats.throttle_429.load(Ordering::Relaxed),
        server_5xx: retry_stats.server_5xx.load(Ordering::Relaxed),
        transport_err: retry_stats.transport_err.load(Ordering::Relaxed),
    })
}
