use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use azcp::{
    parse_location, BlobClient, Credential, DownloadRange, Location, TransferConfig, TransferEngine,
};

use crate::cli::Args;

pub fn run(args: &Args, my_ranges: Vec<DownloadRange>, max_bandwidth: Option<u64>) -> Result<()> {
    if my_ranges.is_empty() {
        return Ok(());
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
    Ok(())
}
