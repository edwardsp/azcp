use std::path::Path;
use std::sync::Arc;

use crate::auth::Credential;
use crate::config::TransferConfig;
use crate::engine::progress::TransferProgress;
use crate::engine::{TransferEngine, TransferSummary};
use crate::error::{AzcpError, Result};
use crate::storage::blob::client::BlobClient;
use crate::storage::location::{self, BlobLocation, Location};

use super::args::CopyArgs;

pub async fn run(args: &CopyArgs) -> Result<()> {
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
        progress: args.progress,
        max_retries: args.max_retries,
        shard: args.shard,
    };

    match (&source, &dest) {
        (Location::Local(src), Location::AzureBlob(dst)) => {
            if config.discard {
                return Err(AzcpError::Transfer(
                    "--discard is only valid for downloads (blob -> local)".into(),
                ));
            }
            upload(src, dst, config).await
        }
        (Location::AzureBlob(src), Location::Local(dst)) => download(src, dst, config).await,
        (Location::AzureBlob(_), Location::AzureBlob(_)) => {
            eprintln!("Server-to-server copy not yet implemented.");
            Ok(())
        }
        (Location::Local(_), Location::Local(_)) => {
            eprintln!("Local-to-local copy not supported. Use cp.");
            Ok(())
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
) -> Result<()> {
    let credential = resolve_credential(dest)?;
    let client = BlobClient::with_max_retries(credential, config.max_retries)?;
    let engine = Arc::new(TransferEngine::new(client, config)?);

    let src_path = Path::new(source);

    if src_path.is_dir() {
        let summary = engine
            .upload_directory(src_path, &dest.account, &dest.container, &dest.path)
            .await?;
        print_summary("Upload", &summary);
        print_retry_stats(engine.client());
        check_summary(&summary)?;
    } else if src_path.is_file() {
        let blob_path = if dest.path.is_empty() {
            src_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        } else {
            dest.path.clone()
        };

        let metadata = std::fs::metadata(src_path)?;
        let progress = Arc::new(TransferProgress::new(1, metadata.len(), engine.config().progress));
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
    } else {
        eprintln!("Source not found: {source}");
    }

    Ok(())
}

async fn download(
    source: &BlobLocation,
    dest: &str,
    config: TransferConfig,
) -> Result<()> {
    let credential = resolve_credential(source)?;
    let client = BlobClient::with_max_retries(credential, config.max_retries)?;
    let engine = Arc::new(TransferEngine::new(client, config)?);

    let dest_path = Path::new(dest);

    if source.path.is_empty() || source.path.ends_with('/') {
        let summary = engine
            .download_directory(
                &source.account,
                &source.container,
                &source.path,
                dest_path,
            )
            .await?;
        print_summary("Download", &summary);
        print_retry_stats(engine.client());
        check_summary(&summary)?;
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

        let progress = Arc::new(TransferProgress::new(1, props.content_length, engine.config().progress));
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
    }

    Ok(())
}

fn resolve_credential(loc: &BlobLocation) -> Result<Credential> {
    Credential::resolve_or_anonymous(&loc.account, loc.sas_token.as_deref())
}

fn print_summary(op: &str, s: &TransferSummary) {
    println!(
        "\n{op} complete: {} files, {}",
        s.total_files,
        humansize::format_size(s.total_bytes, humansize::BINARY)
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
    let s = client.retry_stats();
    let t503 = s.throttle_503.load(std::sync::atomic::Ordering::Relaxed);
    let t429 = s.throttle_429.load(std::sync::atomic::Ordering::Relaxed);
    let t5xx = s.server_5xx.load(std::sync::atomic::Ordering::Relaxed);
    let txerr = s.transport_err.load(std::sync::atomic::Ordering::Relaxed);
    if t503 + t429 + t5xx + txerr > 0 {
        println!(
            "  Retries:   503x{t503} 429x{t429} 5xxx{t5xx} transport-err x{txerr}"
        );
    } else {
        println!("  Retries:   none");
    }
}
