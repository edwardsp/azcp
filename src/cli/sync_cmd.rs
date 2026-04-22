use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::auth::Credential;
use crate::config::TransferConfig;
use crate::engine::{build_glob_set, TransferEngine};
use crate::error::Result;
use crate::storage::blob::client::BlobClient;
use crate::storage::blob::models::BlobItem;
use crate::storage::local::{self, LocalEntry};
use crate::storage::location::{self, BlobLocation, Location};

use super::args::{CompareMethod, SyncArgs};

pub async fn run(args: &SyncArgs) -> Result<()> {
    let source = location::parse_location(&args.source)?;
    let dest = location::parse_location(&args.destination)?;

    match (&source, &dest) {
        (Location::Local(src), Location::AzureBlob(dst)) => {
            sync_local_to_blob(src, dst, args).await
        }
        (Location::AzureBlob(src), Location::Local(dst)) => {
            sync_blob_to_local(src, dst, args).await
        }
        _ => {
            eprintln!("Sync only supports local <-> Azure Blob.");
            Ok(())
        }
    }
}

fn build_engine(args: &SyncArgs, credential: Credential) -> Result<Arc<TransferEngine>> {
    let client = BlobClient::with_max_retries(credential, args.max_retries)?;
    let config = TransferConfig {
        recursive: true,
        overwrite: true,
        block_size: args.block_size,
        concurrency: args.concurrency,
        parallel_files: args.parallel_files,
        dry_run: args.dry_run,
        discard: false,
        check_md5: false,
        include_pattern: args.include_pattern.clone(),
        exclude_pattern: args.exclude_pattern.clone(),
        progress: args.progress,
        max_retries: args.max_retries,
        shard: args.shard,
        shardlist: None,
    };
    Ok(Arc::new(TransferEngine::new(client, config)?))
}

fn glob_filter_local(args: &SyncArgs, files: Vec<LocalEntry>) -> Result<Vec<LocalEntry>> {
    let include = build_glob_set(args.include_pattern.as_deref())?;
    let exclude = build_glob_set(args.exclude_pattern.as_deref())?;
    Ok(files
        .into_iter()
        .filter(|f| {
            if let Some(inc) = &include {
                if !inc.is_match(&f.relative_path) {
                    return false;
                }
            }
            if let Some(exc) = &exclude {
                if exc.is_match(&f.relative_path) {
                    return false;
                }
            }
            true
        })
        .collect())
}

fn glob_filter_blobs(
    args: &SyncArgs,
    blobs: Vec<BlobItem>,
    prefix: &str,
) -> Result<Vec<BlobItem>> {
    let include = build_glob_set(args.include_pattern.as_deref())?;
    let exclude = build_glob_set(args.exclude_pattern.as_deref())?;
    Ok(blobs
        .into_iter()
        .filter(|b| {
            let relative = b
                .name
                .strip_prefix(prefix)
                .unwrap_or(&b.name)
                .trim_start_matches('/');
            if let Some(inc) = &include {
                if !inc.is_match(relative) {
                    return false;
                }
            }
            if let Some(exc) = &exclude {
                if exc.is_match(relative) {
                    return false;
                }
            }
            true
        })
        .collect())
}

fn parse_blob_lmt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn system_time_to_utc(t: SystemTime) -> Option<DateTime<Utc>> {
    Some(DateTime::<Utc>::from(t))
}

async fn md5_of_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let arr: [u8; 16] = hasher.finalize().into();
    Ok(STANDARD.encode(arr))
}

struct RemoteRecord {
    size: u64,
    last_modified: Option<DateTime<Utc>>,
    md5_b64: Option<String>,
}

fn remote_index(blobs: &[BlobItem], prefix: &str) -> HashMap<String, RemoteRecord> {
    blobs
        .iter()
        .map(|b| {
            let name = b
                .name
                .strip_prefix(prefix)
                .unwrap_or(&b.name)
                .trim_start_matches('/')
                .to_string();
            let props = b.properties.as_ref();
            let size = props.and_then(|p| p.content_length).unwrap_or(0);
            let last_modified = props
                .and_then(|p| p.last_modified.as_deref())
                .and_then(parse_blob_lmt);
            let md5_b64 = props.and_then(|p| p.content_md5.clone());
            (
                name,
                RemoteRecord {
                    size,
                    last_modified,
                    md5_b64,
                },
            )
        })
        .collect()
}

fn md5_preflight(
    local_files: &[LocalEntry],
    remote_map: &HashMap<String, RemoteRecord>,
) -> Result<()> {
    let missing: Vec<&str> = local_files
        .iter()
        .filter_map(|f| {
            let r = remote_map.get(&f.relative_path)?;
            if r.size > 0 && r.md5_b64.is_none() {
                Some(f.relative_path.as_str())
            } else {
                None
            }
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let sample: Vec<String> = missing.iter().take(10).map(|s| s.to_string()).collect();
    let extra = missing.len().saturating_sub(sample.len());
    let extra_note = if extra > 0 {
        format!("\n  ... and {extra} more")
    } else {
        String::new()
    };
    Err(crate::error::AzcpError::Transfer(format!(
        "--compare-method md5 requires Content-MD5 on all remote blobs, but {} blob(s) are missing it:\n  - {}{extra_note}\nhint: re-upload these with 'azcp copy --check-md5 ...' to populate Content-MD5, or use --compare-method size-and-mtime",
        missing.len(),
        sample.join("\n  - "),
    )))
}

async fn needs_upload(
    method: CompareMethod,
    local: &LocalEntry,
    remote: Option<&RemoteRecord>,
) -> Result<bool> {
    let Some(r) = remote else {
        return Ok(true);
    };
    match method {
        CompareMethod::Always => Ok(true),
        CompareMethod::Size => Ok(r.size != local.size),
        CompareMethod::SizeAndMtime => {
            if r.size != local.size {
                return Ok(true);
            }
            let local_mtime = local.modified.and_then(system_time_to_utc);
            match (local_mtime, r.last_modified) {
                // Blob Last-Modified is RFC2822 with 1-second precision, so
                // compare at whole-second granularity to avoid treating a
                // freshly-uploaded blob as older than its nanosecond-precise
                // source file.
                (Some(lm), Some(rm)) => Ok(lm.timestamp() > rm.timestamp()),
                _ => Ok(true),
            }
        }
        CompareMethod::Md5 => {
            if r.size != local.size {
                return Ok(true);
            }
            let Some(remote_md5) = &r.md5_b64 else {
                return Ok(true);
            };
            let actual = md5_of_file(&local.path).await?;
            Ok(&actual != remote_md5)
        }
    }
}

async fn needs_download(
    method: CompareMethod,
    remote: &BlobItem,
    remote_size: u64,
    remote_lmt: Option<DateTime<Utc>>,
    remote_md5: Option<&str>,
    local: Option<&LocalEntry>,
) -> Result<bool> {
    let Some(l) = local else {
        return Ok(true);
    };
    match method {
        CompareMethod::Always => Ok(true),
        CompareMethod::Size => Ok(l.size != remote_size),
        CompareMethod::SizeAndMtime => {
            if l.size != remote_size {
                return Ok(true);
            }
            let local_mtime = l.modified.and_then(system_time_to_utc);
            match (local_mtime, remote_lmt) {
                (Some(lm), Some(rm)) => Ok(rm > lm),
                _ => Ok(true),
            }
        }
        CompareMethod::Md5 => {
            if l.size != remote_size {
                return Ok(true);
            }
            let Some(remote_md5) = remote_md5 else {
                let _ = remote;
                return Ok(true);
            };
            let actual = md5_of_file(&l.path).await?;
            Ok(actual != remote_md5)
        }
    }
}

async fn sync_local_to_blob(
    source: &str,
    dest: &BlobLocation,
    args: &SyncArgs,
) -> Result<()> {
    let credential = resolve_credential(dest)?;
    let engine = build_engine(args, credential)?;

    let local_files =
        glob_filter_local(args, local::walk_directory(Path::new(source), true).await?)?;
    let remote_blobs = engine
        .client()
        .list_blobs(&dest.account, &dest.container, Some(&dest.path), true)
        .await?;
    let remote_blobs = glob_filter_blobs(args, remote_blobs, &dest.path)?;
    let remote_map = remote_index(&remote_blobs, &dest.path);

    if matches!(args.compare_method, CompareMethod::Md5) {
        md5_preflight(&local_files, &remote_map)?;
    }

    // Diff (potentially I/O-heavy for md5 mode) - run in parallel.
    let diff_sem = Arc::new(Semaphore::new(args.concurrency));
    let mut diff_tasks: JoinSet<Result<Option<LocalEntry>>> = JoinSet::new();
    for file in local_files {
        let permit = diff_sem.clone().acquire_owned().await.unwrap();
        let method = args.compare_method;
        let remote = remote_map.get(&file.relative_path).map(|r| RemoteRecord {
            size: r.size,
            last_modified: r.last_modified,
            md5_b64: r.md5_b64.clone(),
        });
        diff_tasks.spawn(async move {
            let _permit = permit;
            let needs = needs_upload(method, &file, remote.as_ref()).await?;
            Ok(if needs { Some(file) } else { None })
        });
    }

    let mut to_upload = Vec::new();
    let mut skipped = 0u64;
    while let Some(j) = diff_tasks.join_next().await {
        match j {
            Ok(Ok(Some(f))) => to_upload.push(f),
            Ok(Ok(None)) => skipped += 1,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(crate::error::AzcpError::Transfer(e.to_string())),
        }
    }

    let to_upload_count = to_upload.len() as u64;
    let summary = engine
        .upload_entries(to_upload, &dest.account, &dest.container, &dest.path)
        .await?;

    if args.delete_destination {
        let live_local: HashSet<String> = local::walk_directory(Path::new(source), true)
            .await?
            .into_iter()
            .map(|f| f.relative_path)
            .collect();
        let mut to_delete = Vec::new();
        for name in remote_map.keys() {
            if !live_local.contains(name) {
                let blob_path = if dest.path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", dest.path.trim_end_matches('/'))
                };
                to_delete.push(blob_path);
            }
        }
        delete_remote_parallel(engine.client(), &dest.account, &dest.container, to_delete, args)
            .await?;
    }

    println!(
        "\nSync complete: {} planned, {} uploaded, {} unchanged",
        to_upload_count, summary.succeeded, skipped
    );
    if summary.failed > 0 {
        println!("  Failed:    {}", summary.failed);
        return Err(crate::error::AzcpError::Transfer(format!(
            "{} file(s) failed to transfer",
            summary.failed
        )));
    }
    Ok(())
}

async fn sync_blob_to_local(
    source: &BlobLocation,
    dest: &str,
    args: &SyncArgs,
) -> Result<()> {
    let credential = resolve_credential(source)?;
    let engine = build_engine(args, credential)?;

    let remote_blobs = engine
        .client()
        .list_blobs(&source.account, &source.container, Some(&source.path), true)
        .await?;
    let remote_blobs = glob_filter_blobs(args, remote_blobs, &source.path)?;

    let dest_path = Path::new(dest);
    let local_files = if dest_path.exists() {
        glob_filter_local(args, local::walk_directory(dest_path, true).await?)?
    } else {
        Vec::new()
    };
    let local_map: HashMap<String, LocalEntry> = local_files
        .into_iter()
        .map(|f| (f.relative_path.clone(), f))
        .collect();

    if matches!(args.compare_method, CompareMethod::Md5) {
        let missing: Vec<&str> = remote_blobs
            .iter()
            .filter(|b| {
                let props = b.properties.as_ref();
                let size = props.and_then(|p| p.content_length).unwrap_or(0);
                let md5 = props.and_then(|p| p.content_md5.as_ref());
                size > 0 && md5.is_none()
            })
            .map(|b| b.name.as_str())
            .collect();
        if !missing.is_empty() {
            let sample: Vec<String> =
                missing.iter().take(10).map(|s| s.to_string()).collect();
            let extra = missing.len().saturating_sub(sample.len());
            let extra_note = if extra > 0 {
                format!("\n  ... and {extra} more")
            } else {
                String::new()
            };
            return Err(crate::error::AzcpError::Transfer(format!(
                "--compare-method md5 requires Content-MD5 on all remote blobs, but {} blob(s) are missing it:\n  - {}{extra_note}\nhint: re-upload with 'azcp copy --check-md5 ...' or use --compare-method size-and-mtime",
                missing.len(),
                sample.join("\n  - "),
            )));
        }
    }

    let diff_sem = Arc::new(Semaphore::new(args.concurrency));
    let mut diff_tasks: JoinSet<Result<Option<BlobItem>>> = JoinSet::new();
    for blob in remote_blobs.clone() {
        let permit = diff_sem.clone().acquire_owned().await.unwrap();
        let method = args.compare_method;
        let relative = blob
            .name
            .strip_prefix(&source.path)
            .unwrap_or(&blob.name)
            .trim_start_matches('/')
            .to_string();
        let local_entry = local_map.get(&relative).cloned();
        let props = blob.properties.clone();
        diff_tasks.spawn(async move {
            let _permit = permit;
            let size = props.as_ref().and_then(|p| p.content_length).unwrap_or(0);
            let lmt = props
                .as_ref()
                .and_then(|p| p.last_modified.as_deref())
                .and_then(parse_blob_lmt);
            let md5 = props.as_ref().and_then(|p| p.content_md5.clone());
            let needs = needs_download(
                method,
                &blob,
                size,
                lmt,
                md5.as_deref(),
                local_entry.as_ref(),
            )
            .await?;
            Ok(if needs { Some(blob) } else { None })
        });
    }

    let mut to_download = Vec::new();
    let mut skipped = 0u64;
    while let Some(j) = diff_tasks.join_next().await {
        match j {
            Ok(Ok(Some(b))) => to_download.push(b),
            Ok(Ok(None)) => skipped += 1,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(crate::error::AzcpError::Transfer(e.to_string())),
        }
    }

    let to_download_count = to_download.len() as u64;
    let summary = engine
        .download_entries(
            to_download,
            &source.account,
            &source.container,
            &source.path,
            dest_path,
        )
        .await?;

    if args.delete_destination {
        let remote_set: HashSet<String> = remote_blobs
            .iter()
            .map(|b| {
                b.name
                    .strip_prefix(&source.path)
                    .unwrap_or(&b.name)
                    .trim_start_matches('/')
                    .to_string()
            })
            .collect();

        let mut to_delete: Vec<PathBuf> = Vec::new();
        for (name, entry) in &local_map {
            if !remote_set.contains(name) {
                to_delete.push(entry.path.clone());
            }
        }
        delete_local_parallel(to_delete, args).await?;
    }

    println!(
        "\nSync complete: {} planned, {} downloaded, {} unchanged",
        to_download_count, summary.succeeded, skipped
    );
    if summary.failed > 0 {
        println!("  Failed:    {}", summary.failed);
        return Err(crate::error::AzcpError::Transfer(format!(
            "{} file(s) failed to transfer",
            summary.failed
        )));
    }
    Ok(())
}

async fn delete_remote_parallel(
    client: &BlobClient,
    account: &str,
    container: &str,
    paths: Vec<String>,
    args: &SyncArgs,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        for p in &paths {
            println!("[dry-run] delete remote: {p}");
        }
        return Ok(());
    }
    let sem = Arc::new(Semaphore::new(args.concurrency));
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    let client = Arc::new(client.clone());
    for path in paths {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let acct = account.to_string();
        let ctr = container.to_string();
        tasks.spawn(async move {
            let _permit = permit;
            client.delete_blob(&acct, &ctr, &path).await
        });
    }
    while let Some(j) = tasks.join_next().await {
        match j {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("delete failed: {e}"),
            Err(e) => eprintln!("delete failed: {e}"),
        }
    }
    Ok(())
}

async fn delete_local_parallel(paths: Vec<PathBuf>, args: &SyncArgs) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        for p in &paths {
            println!("[dry-run] delete local: {}", p.display());
        }
        return Ok(());
    }
    for path in paths {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            eprintln!("delete failed {}: {e}", path.display());
        }
    }
    Ok(())
}

fn resolve_credential(loc: &BlobLocation) -> Result<Credential> {
    Credential::resolve_or_anonymous(&loc.account, loc.sas_token.as_deref())
}
