use std::path::Path;
use std::sync::Arc;

use crate::auth::Credential;
use crate::config::TransferConfig;
use crate::engine::progress::TransferProgress;
use crate::engine::rate_limiter::RateLimiter;
use crate::engine::{TransferEngine, TransferSummary};
use crate::error::{AzcpError, Result};
use crate::storage::blob::client::{BlobClient, LatencyStats, RetryStats};
use crate::storage::location::{self, BlobLocation, Location};

use super::args::{resolve_progress, CopyArgs};

#[derive(Clone, Default)]
pub struct SharedTransfer {
    pub progress: Option<Arc<TransferProgress>>,
    pub retry_stats: Option<Arc<RetryStats>>,
    pub latency_stats: Option<Arc<LatencyStats>>,
    pub rate_limiter: Option<Arc<RateLimiter>>,
}

pub async fn run(args: &CopyArgs) -> Result<()> {
    run_with_shared(args, SharedTransfer::default())
        .await
        .map(|_| ())
}

pub async fn run_with_shared(
    args: &CopyArgs,
    shared: SharedTransfer,
) -> Result<Option<TransferSummary>> {
    let source = location::parse_location(&args.source)?;
    let dest = location::parse_location(&args.destination)?;

    let config = TransferConfig {
        block_size: args.block_size,
        concurrency: args.concurrency,
        parallel_files: args.parallel_files,
        dry_run: args.dry_run,
        discard: args.discard,
        overwrite: !args.no_overwrite,
        recursive: args.recursive,
        include_pattern: args.include_pattern.clone(),
        exclude_pattern: args.exclude_pattern.clone(),
        check_md5: args.check_md5,
        progress: resolve_progress(args.progress, args.no_progress),
        max_retries: args.max_retries,
        shard: args.shard,
        shardlist: args.shardlist.clone(),
        max_bandwidth_bytes_per_sec: args.max_bandwidth,
    };

    match (&source, &dest) {
        (Location::Local(src), Location::AzureBlob(dst)) => {
            if config.discard {
                return Err(AzcpError::Transfer(
                    "--discard is only valid for downloads (blob -> local)".into(),
                ));
            }
            if config.shardlist.is_some() {
                return Err(AzcpError::Transfer(
                    "--shardlist is only valid for downloads (blob -> local)".into(),
                ));
            }
            upload(src, dst, config, shared).await
        }
        (Location::AzureBlob(src), Location::Local(dst)) => {
            download(src, dst, config, shared).await
        }
        (Location::AzureBlob(_), Location::AzureBlob(_)) => {
            eprintln!("Server-to-server copy not yet implemented.");
            Ok(None)
        }
        (Location::Local(_), Location::Local(_)) => {
            eprintln!("Local-to-local copy not supported. Use cp.");
            Ok(None)
        }
    }
}

fn check_summary(s: &TransferSummary) -> Result<()> {
    if s.failed > 0 {
        return Err(AzcpError::Transfer(format!(
            "{} file(s) failed to transfer",
            s.failed
        )));
    }
    Ok(())
}

async fn upload(
    source: &str,
    dest: &BlobLocation,
    config: TransferConfig,
    shared: SharedTransfer,
) -> Result<Option<TransferSummary>> {
    let credential = resolve_credential(dest)?;
    let client = build_client(credential, config.max_retries, &shared)?;
    let mut engine = TransferEngine::new(client, config)?;
    if let Some(p) = &shared.progress {
        engine = engine.with_shared_progress(p.clone());
    }
    if let Some(rl) = &shared.rate_limiter {
        engine = engine.with_rate_limiter(rl.clone());
    }
    let engine = Arc::new(engine);

    let src_path = Path::new(source);

    if src_path.is_dir() {
        let started = std::time::Instant::now();
        let summary = engine
            .upload_directory(src_path, &dest.account, &dest.container, &dest.path)
            .await?;
        if shared.progress.is_none() {
            print_summary("Upload", &summary, started.elapsed());
            print_retry_stats(engine.client());
        }
        check_summary(&summary)?;
        Ok(Some(summary))
    } else if src_path.is_file() {
        let blob_path = if dest.path.is_empty() {
            src_path.file_name().unwrap().to_string_lossy().to_string()
        } else {
            dest.path.clone()
        };

        let metadata = std::fs::metadata(src_path)?;
        let progress = Arc::new(TransferProgress::new(
            1,
            metadata.len(),
            engine.config().progress,
        ));
        progress.attach_retry_stats(engine.client().retry_stats());
        let pb = progress.create_file_bar(metadata.len());
        pb.set_message(blob_path.clone());

        let result = engine
            .upload_file(
                src_path,
                &dest.account,
                &dest.container,
                &blob_path,
                Some(&pb),
            )
            .await;
        pb.finish();
        progress.finish();

        match result {
            Ok(size) => println!(
                "Transferred: {} ({})",
                blob_path,
                humansize::format_size(size, humansize::BINARY)
            ),
            Err(AzcpError::AlreadyExists(name)) => {
                println!("Skipped (already exists): {name}");
            }
            Err(e) => return Err(e),
        }
        Ok(None)
    } else {
        eprintln!("Source not found: {source}");
        Ok(None)
    }
}

async fn download(
    source: &BlobLocation,
    dest: &str,
    config: TransferConfig,
    shared: SharedTransfer,
) -> Result<Option<TransferSummary>> {
    let credential = resolve_credential(source)?;
    let client = build_client(credential, config.max_retries, &shared)?;
    let mut engine = TransferEngine::new(client, config)?;
    if let Some(p) = &shared.progress {
        engine = engine.with_shared_progress(p.clone());
    }
    if let Some(rl) = &shared.rate_limiter {
        engine = engine.with_rate_limiter(rl.clone());
    }
    let engine = Arc::new(engine);

    let dest_path = Path::new(dest);

    if source.path.is_empty() || source.path.ends_with('/') {
        let started = std::time::Instant::now();
        let summary = engine
            .download_directory(&source.account, &source.container, &source.path, dest_path)
            .await?;
        if shared.progress.is_none() {
            print_summary("Download", &summary, started.elapsed());
            print_retry_stats(engine.client());
        }
        check_summary(&summary)?;
        Ok(Some(summary))
    } else {
        let file_name = Path::new(&source.path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| source.path.clone());

        let local_path = if dest_path.is_dir() {
            dest_path.join(&file_name)
        } else {
            dest_path.to_path_buf()
        };

        let props = engine
            .client()
            .get_blob_properties(&source.account, &source.container, &source.path)
            .await?;

        let progress = Arc::new(TransferProgress::new(
            1,
            props.content_length,
            engine.config().progress,
        ));
        progress.attach_retry_stats(engine.client().retry_stats());
        let pb = progress.create_file_bar(props.content_length);
        pb.set_message(source.path.clone());

        let result = engine
            .download_file(
                &source.account,
                &source.container,
                &source.path,
                &local_path,
                Some(props.content_length),
                props.content_md5.as_deref(),
                Some(&pb),
            )
            .await;
        pb.finish();
        progress.finish();

        let size = result?;
        println!(
            "Transferred: {} ({})",
            local_path.display(),
            humansize::format_size(size, humansize::BINARY)
        );
        Ok(None)
    }
}

fn build_client(
    credential: Credential,
    max_retries: u32,
    shared: &SharedTransfer,
) -> Result<BlobClient> {
    match (&shared.retry_stats, &shared.latency_stats) {
        (Some(r), Some(l)) => {
            BlobClient::with_shared_stats(credential, max_retries, r.clone(), l.clone())
        }
        _ => BlobClient::with_max_retries(credential, max_retries),
    }
}

fn resolve_credential(loc: &BlobLocation) -> Result<Credential> {
    Credential::resolve_or_anonymous(&loc.account, loc.sas_token.as_deref())
}

pub fn print_summary(op: &str, s: &TransferSummary, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs_f64();
    let gbps = if secs > 0.0 {
        (s.total_bytes as f64 * 8.0) / (secs * 1e9)
    } else {
        0.0
    };
    println!(
        "\n{op} complete: {} files, {} in {:.1}s = {:.2} Gbps",
        s.total_files,
        humansize::format_size(s.total_bytes, humansize::BINARY),
        secs,
        gbps
    );
    println!("  Succeeded: {}", s.succeeded);
    if s.failed > 0 {
        println!("  Failed:    {}", s.failed);
    }
    if s.skipped > 0 {
        println!("  Skipped:   {}", s.skipped);
    }
}

fn print_retry_stats(client: &BlobClient) {
    print_stats(&client.retry_stats(), &client.latency_stats());
}

pub fn print_stats(retry: &RetryStats, latency: &LatencyStats) {
    let s = retry;
    let t503 = s.throttle_503.load(std::sync::atomic::Ordering::Relaxed);
    let t429 = s.throttle_429.load(std::sync::atomic::Ordering::Relaxed);
    let t5xx = s.server_5xx.load(std::sync::atomic::Ordering::Relaxed);
    let txerr = s.transport_err.load(std::sync::atomic::Ordering::Relaxed);
    if t503 + t429 + t5xx + txerr > 0 {
        println!("  Retries:   503x{t503} 429x{t429} 5xxx{t5xx} transport-err x{txerr}");
    } else {
        println!("  Retries:   none");
    }

    let l = latency;
    let count = l.count.load(std::sync::atomic::Ordering::Relaxed);
    if count == 0 {
        return;
    }
    let sum_us = l.sum_us.load(std::sync::atomic::Ordering::Relaxed);
    let min_us = l.min_us.load(std::sync::atomic::Ordering::Relaxed);
    let max_us = l.max_us.load(std::sync::atomic::Ordering::Relaxed);
    let bytes = l.bytes.load(std::sync::atomic::Ordering::Relaxed);
    let mean_ms = (sum_us as f64) / (count as f64) / 1000.0;
    let min_ms = (min_us as f64) / 1000.0;
    let max_ms = (max_us as f64) / 1000.0;
    let p50 = l.percentile_ms(0.50);
    let p95 = l.percentile_ms(0.95);
    let p99 = l.percentile_ms(0.99);
    let avg_bytes_per_req = (bytes as f64) / (count as f64);
    let avg_per_req_mbps = (avg_bytes_per_req * 8.0) / ((mean_ms / 1000.0) * 1e6);
    println!(
        "  Latency:   count={count} mean={mean_ms:.1}ms min={min_ms:.1}ms max={max_ms:.1}ms p50<={p50}ms p95<={p95}ms p99<={p99}ms"
    );
    println!(
        "  Per-req:   {} avg/req @ {avg_per_req_mbps:.0} Mbps/req",
        humansize::format_size(avg_bytes_per_req as u64, humansize::BINARY)
    );
}
