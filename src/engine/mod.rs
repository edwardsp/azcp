pub mod progress;

use std::path::Path;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use globset::{Glob, GlobSet, GlobSetBuilder};
use md5::{Digest, Md5};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::TransferConfig;
use crate::error::{AzcpError, Result};
use crate::storage::blob::client::BlobClient;
use crate::storage::blob::models::{BlockListEntry, UploadOptions};
use crate::storage::local;

use self::progress::TransferProgress;

pub struct TransferEngine {
    client: Arc<BlobClient>,
    config: TransferConfig,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    // Global cap on in-flight HTTP requests across all files and blocks.
    // Held only at the HTTP-request boundary, never at file level, to
    // prevent deadlock when nested operations would exceed the pool size.
    semaphore: Arc<Semaphore>,
    // Caps how many files are actively being transferred concurrently.
    // Each file's chunks still share the global `semaphore` budget, but
    // multiple files race for those permits in parallel - removing the
    // file-by-file dispatch bottleneck.
    file_semaphore: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
pub struct TransferSummary {
    pub total_files: u64,
    pub total_bytes: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped: u64,
}

impl TransferEngine {
    pub fn new(client: BlobClient, config: TransferConfig) -> Result<Self> {
        let include = build_glob_set(config.include_pattern.as_deref())?;
        let exclude = build_glob_set(config.exclude_pattern.as_deref())?;
        let semaphore = Arc::new(Semaphore::new(config.concurrency));
        let file_semaphore = Arc::new(Semaphore::new(config.parallel_files.max(1)));
        Ok(Self {
            client: Arc::new(client),
            config,
            include,
            exclude,
            semaphore,
            file_semaphore,
        })
    }

    pub fn client(&self) -> &BlobClient {
        &self.client
    }

    pub fn config(&self) -> &TransferConfig {
        &self.config
    }

    fn upload_options(&self, content_md5: Option<[u8; 16]>) -> UploadOptions {
        UploadOptions {
            overwrite: self.config.overwrite,
            content_md5,
        }
    }

    fn path_matches(&self, relative: &str) -> bool {
        if let Some(inc) = &self.include {
            if !inc.is_match(relative) {
                return false;
            }
        }
        if let Some(exc) = &self.exclude {
            if exc.is_match(relative) {
                return false;
            }
        }
        true
    }

    pub async fn upload_file(
        &self,
        local_path: &Path,
        account: &str,
        container: &str,
        blob_path: &str,
        progress_bar: Option<&indicatif::ProgressBar>,
    ) -> Result<u64> {
        let metadata = tokio::fs::metadata(local_path).await?;
        let file_size = metadata.len();

        if file_size < self.config.block_size {
            let data = tokio::fs::read(local_path).await?;
            let ct = mime_from_path(local_path);
            let md5 = if self.config.check_md5 {
                Some(md5_of(&data))
            } else {
                None
            };
            let _permit = self.semaphore.acquire().await.unwrap();
            self.client
                .put_blob(
                    account,
                    container,
                    blob_path,
                    Bytes::from(data),
                    Some(&ct),
                    &self.upload_options(md5),
                )
                .await?;
            if let Some(pb) = progress_bar {
                pb.inc(file_size);
            }
        } else {
            self.upload_blocks(
                local_path,
                account,
                container,
                blob_path,
                file_size,
                progress_bar,
            )
            .await?;
        }

        Ok(file_size)
    }

    async fn upload_blocks(
        &self,
        local_path: &Path,
        account: &str,
        container: &str,
        blob_path: &str,
        file_size: u64,
        progress_bar: Option<&indicatif::ProgressBar>,
    ) -> Result<()> {
        let block_size = self.config.block_size as usize;
        let num_blocks = (file_size as usize + block_size - 1) / block_size;
        let check_md5 = self.config.check_md5;

        let mut block_entries = Vec::with_capacity(num_blocks);
        for i in 0..num_blocks {
            // Azure requires base64-encoded block IDs of equal length.
            // Encode once so the put_block URL and the BlockList XML agree.
            block_entries.push(BlockListEntry {
                id: STANDARD.encode(format!("{i:06}")),
            });
        }
        let block_ids: Vec<String> = block_entries.iter().map(|e| e.id.clone()).collect();

        // Bounded channel: caps prefetched blocks in RAM to ~concurrency.
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(String, Bytes)>(self.config.concurrency);

        let producer_path = local_path.to_path_buf();
        let producer = tokio::spawn(async move {
            let mut file = tokio::fs::File::open(&producer_path).await?;
            let mut hasher = if check_md5 { Some(Md5::new()) } else { None };
            let mut remaining = file_size as usize;
            for id in block_ids {
                let this_size = std::cmp::min(block_size, remaining);
                let mut buf = vec![0u8; this_size];
                file.read_exact(&mut buf).await?;
                if let Some(h) = hasher.as_mut() {
                    h.update(&buf);
                }
                if tx.send((id, Bytes::from(buf))).await.is_err() {
                    break;
                }
                remaining -= this_size;
            }
            let md5 = hasher.map(|h| {
                let arr: [u8; 16] = h.finalize().into();
                arr
            });
            Ok::<Option<[u8; 16]>, AzcpError>(md5)
        });

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();
        while let Some((block_id, chunk)) = rx.recv().await {
            let client = self.client.clone();
            let sem = self.semaphore.clone();
            let acct = account.to_string();
            let ctr = container.to_string();
            let bp = blob_path.to_string();
            let pb = progress_bar.cloned();
            tasks.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let chunk_len = chunk.len() as u64;
                client.put_block(&acct, &ctr, &bp, &block_id, chunk).await?;
                if let Some(pb) = &pb {
                    pb.inc(chunk_len);
                }
                Ok(())
            });
        }
        while let Some(res) = tasks.join_next().await {
            res.map_err(|e| AzcpError::Transfer(e.to_string()))??;
        }

        let whole_md5 = producer
            .await
            .map_err(|e| AzcpError::Transfer(e.to_string()))??;

        let ct = mime_from_path(local_path);
        let _permit = self.semaphore.acquire().await.unwrap();
        self.client
            .put_block_list(
                account,
                container,
                blob_path,
                &block_entries,
                Some(&ct),
                &self.upload_options(whole_md5),
            )
            .await?;
        drop(_permit);

        Ok(())
    }

    pub async fn download_file(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        local_path: &Path,
        expected_size: Option<u64>,
        expected_md5: Option<&str>,
        progress_bar: Option<&indicatif::ProgressBar>,
    ) -> Result<u64> {
        local::ensure_parent_dir(local_path).await?;

        let size = expected_size.unwrap_or(0);

        let written = if size < self.config.block_size {
            let permit = self.semaphore.acquire().await.unwrap();
            let data = self.client.get_blob(account, container, blob_path).await?;
            drop(permit);
            let len = data.len() as u64;
            if self.config.check_md5 {
                verify_md5(blob_path, expected_md5, &data)?;
            }
            tokio::fs::write(local_path, &data).await?;
            if let Some(pb) = progress_bar {
                pb.inc(len);
            }
            len
        } else {
            self.download_chunks(
                account,
                container,
                blob_path,
                local_path,
                size,
                expected_md5,
                progress_bar,
            )
            .await?
        };

        Ok(written)
    }

    async fn download_chunks(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        local_path: &Path,
        file_size: u64,
        expected_md5: Option<&str>,
        progress_bar: Option<&indicatif::ProgressBar>,
    ) -> Result<u64> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        let block_size = self.config.block_size;
        let num_chunks = (file_size + block_size - 1) / block_size;

        // Pre-allocate the file so workers can write to non-overlapping
        // ranges in parallel without coordinating on file length.
        let file = tokio::fs::File::create(local_path).await?;
        file.set_len(file_size).await?;
        drop(file);

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();
        for i in 0..num_chunks {
            let offset = i * block_size;
            let length = std::cmp::min(block_size, file_size - offset);

            let client = self.client.clone();
            let sem = self.semaphore.clone();
            let acct = account.to_string();
            let ctr = container.to_string();
            let bp = blob_path.to_string();
            let path = local_path.to_path_buf();
            let pb = progress_bar.cloned();

            tasks.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let data = client
                    .get_blob_range(&acct, &ctr, &bp, offset, length)
                    .await?;
                drop(_permit);

                let mut f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .await?;
                f.seek(std::io::SeekFrom::Start(offset)).await?;
                f.write_all(&data).await?;
                f.flush().await?;

                if let Some(pb) = &pb {
                    pb.inc(data.len() as u64);
                }
                Ok(())
            });
        }

        while let Some(res) = tasks.join_next().await {
            res.map_err(|e| AzcpError::Transfer(e.to_string()))??;
        }

        if self.config.check_md5 {
            verify_md5_file(blob_path, expected_md5, local_path).await?;
        }

        Ok(file_size)
    }

    pub async fn upload_directory(
        self: &Arc<Self>,
        local_dir: &Path,
        account: &str,
        container: &str,
        blob_prefix: &str,
    ) -> Result<TransferSummary> {
        let mut files = local::walk_directory(local_dir, self.config.recursive).await?;
        files.retain(|f| self.path_matches(&f.relative_path));
        apply_shard(&mut files, self.config.shard, |f| f.relative_path.clone(), |f| f.size);
        self.upload_entries(files, account, container, blob_prefix).await
    }

    pub async fn upload_entries(
        self: &Arc<Self>,
        files: Vec<local::LocalEntry>,
        account: &str,
        container: &str,
        blob_prefix: &str,
    ) -> Result<TransferSummary> {
        let total_files = files.len() as u64;
        let total_bytes: u64 = files.iter().map(|f| f.size).sum();

        if self.config.dry_run {
            for f in &files {
                println!("  [dry-run] would upload: {}", f.relative_path);
            }
            return Ok(TransferSummary {
                total_files,
                total_bytes,
                succeeded: total_files,
                failed: 0,
                skipped: 0,
            });
        }

        let progress = Arc::new(TransferProgress::new(
            total_files,
            total_bytes,
            self.config.progress,
        ));
        progress.attach_retry_stats(self.client.retry_stats());
        let mut file_tasks: JoinSet<(String, Result<()>)> = JoinSet::new();
        let block_size = self.config.block_size;
        let check_md5 = self.config.check_md5;

        // Dispatcher walks files in order and, for each file, dispatches all
        // blocks. The acquire_owned() before each spawn provides global
        // back-pressure so total in-flight blocks across all files never
        // exceeds `concurrency`. Block workers do their own parallel disk
        // reads via seek+read_exact, so disk I/O is not serialised by the
        // dispatcher loop.
        for entry in files {
            let blob_path = if blob_prefix.is_empty() {
                entry.relative_path.clone()
            } else {
                format!(
                    "{}/{}",
                    blob_prefix.trim_end_matches('/'),
                    entry.relative_path
                )
            };

            let pb = progress.create_file_bar(entry.size);
            pb.set_message(entry.relative_path.clone());
            let progress_c = progress.clone();
            let file_size = entry.size;

            if file_size == 0 || file_size < block_size {
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let data = tokio::fs::read(&entry.path).await?;
                let md5 = if check_md5 { Some(md5_of(&data)) } else { None };
                let ct = mime_from_path(&entry.path);
                let opts = self.upload_options(md5);
                let client = self.client.clone();
                let acct = account.to_string();
                let ctr = container.to_string();
                let bp = blob_path.clone();
                let pb_c = pb.clone();
                file_tasks.spawn(async move {
                    let _permit = permit;
                    let result = client
                        .put_blob(&acct, &ctr, &bp, Bytes::from(data), Some(&ct), &opts)
                        .await;
                    pb_c.inc(file_size);
                    progress_c.add_bytes(file_size);
                    let r = match result {
                        Ok(_) => {
                            pb_c.finish_and_clear();
                            progress_c.complete_file();
                            Ok(())
                        }
                        Err(e) => {
                            pb_c.finish_and_clear();
                            Err(e)
                        }
                    };
                    (bp, r)
                });
                continue;
            }

            let num_blocks = ((file_size + block_size - 1) / block_size) as usize;
            let block_entries: Vec<BlockListEntry> = (0..num_blocks)
                .map(|i| BlockListEntry {
                    id: STANDARD.encode(format!("{i:06}")),
                })
                .collect();

            // Optional sidecar: when check_md5 is on we need the whole-blob
            // MD5 for x-ms-blob-content-md5. Block reads happen in parallel
            // (out of order) so we can't fold incrementally; instead spawn a
            // sequential reader that runs concurrently with the uploads. It
            // adds disk pressure but keeps the upload path parallel.
            let md5_task: Option<tokio::task::JoinHandle<Result<[u8; 16]>>> = if check_md5 {
                let path = entry.path.clone();
                Some(tokio::spawn(async move {
                    let mut file = tokio::fs::File::open(&path).await?;
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
                    Ok(arr)
                }))
            } else {
                None
            };

            let mut block_handles: Vec<tokio::task::JoinHandle<Result<()>>> =
                Vec::with_capacity(num_blocks);

            for (i, block) in block_entries.iter().enumerate() {
                let offset = (i as u64) * block_size;
                let this_size = std::cmp::min(block_size, file_size - offset) as usize;

                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let block_id = block.id.clone();
                let client = self.client.clone();
                let pb_c = pb.clone();
                let prog_c = progress.clone();
                let acct = account.to_string();
                let ctr = container.to_string();
                let bp = blob_path.clone();
                let path = entry.path.clone();

                block_handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let mut file = tokio::fs::File::open(&path).await?;
                    use tokio::io::AsyncSeekExt;
                    file.seek(std::io::SeekFrom::Start(offset)).await?;
                    let mut buf = vec![0u8; this_size];
                    file.read_exact(&mut buf).await?;
                    let chunk_len = buf.len() as u64;
                    client
                        .put_block(&acct, &ctr, &bp, &block_id, Bytes::from(buf))
                        .await?;
                    pb_c.inc(chunk_len);
                    prog_c.add_bytes(chunk_len);
                    Ok(())
                }));
            }

            let client = self.client.clone();
            let sem = self.semaphore.clone();
            let acct = account.to_string();
            let ctr = container.to_string();
            let bp = blob_path.clone();
            let pb_c = pb.clone();
            let ct = mime_from_path(&entry.path);
            let overwrite = self.config.overwrite;

            file_tasks.spawn(async move {
                let bp_for_err = bp.clone();
                let mut block_err: Option<AzcpError> = None;
                for h in block_handles {
                    match h.await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) if block_err.is_none() => block_err = Some(e),
                        Ok(Err(_)) => {}
                        Err(e) if block_err.is_none() => {
                            block_err = Some(AzcpError::Transfer(e.to_string()))
                        }
                        Err(_) => {}
                    }
                }
                if let Some(e) = block_err {
                    pb_c.finish_and_clear();
                    return (bp_for_err, Err(e));
                }
                let whole_md5 = if let Some(t) = md5_task {
                    match t.await {
                        Ok(Ok(arr)) => Some(arr),
                        Ok(Err(e)) => {
                            pb_c.finish_and_clear();
                            return (bp_for_err, Err(e));
                        }
                        Err(e) => {
                            pb_c.finish_and_clear();
                            return (bp_for_err, Err(AzcpError::Transfer(e.to_string())));
                        }
                    }
                } else {
                    None
                };
                let opts = UploadOptions {
                    overwrite,
                    content_md5: whole_md5,
                };
                let _permit = sem.acquire_owned().await.unwrap();
                let result = client
                    .put_block_list(&acct, &ctr, &bp, &block_entries, Some(&ct), &opts)
                    .await;
                drop(_permit);
                let r = match result {
                    Ok(_) => {
                        pb_c.finish_and_clear();
                        progress_c.complete_file();
                        Ok(())
                    }
                    Err(e) => {
                        pb_c.finish_and_clear();
                        Err(e)
                    }
                };
                (bp_for_err, r)
            });
        }

        let mut succeeded = 0u64;
        let mut failed = 0u64;
        let mut skipped = 0u64;

        while let Some(joined) = file_tasks.join_next().await {
            match joined {
                Ok((_, Ok(()))) => {
                    succeeded += 1;
                }
                Ok((name, Err(AzcpError::AlreadyExists(_)))) => {
                    skipped += 1;
                    progress.println(&format!("skipped (exists): {name}"));
                }
                Ok((name, Err(e))) => {
                    failed += 1;
                    eprintln!("ERROR: {name}: {e}");
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("ERROR: <task join>: {e}");
                }
            }
        }

        progress.finish();

        Ok(TransferSummary {
            total_files,
            total_bytes,
            succeeded,
            failed,
            skipped,
        })
    }

    pub async fn download_directory(
        self: &Arc<Self>,
        account: &str,
        container: &str,
        blob_prefix: &str,
        local_dir: &Path,
    ) -> Result<TransferSummary> {
        let mut blobs = self
            .client
            .list_blobs(account, container, Some(blob_prefix), self.config.recursive)
            .await?;
        blobs.retain(|b| {
            let relative = b
                .name
                .strip_prefix(blob_prefix)
                .unwrap_or(&b.name)
                .trim_start_matches('/');
            self.path_matches(relative)
        });
        apply_shard(
            &mut blobs,
            self.config.shard,
            |b| b.name.clone(),
            |b| b.properties.as_ref().and_then(|p| p.content_length).unwrap_or(0),
        );
        self.download_entries(blobs, account, container, blob_prefix, local_dir).await
    }

    pub async fn download_entries(
        self: &Arc<Self>,
        blobs: Vec<crate::storage::blob::models::BlobItem>,
        account: &str,
        container: &str,
        blob_prefix: &str,
        local_dir: &Path,
    ) -> Result<TransferSummary> {
        let total_files = blobs.len() as u64;
        let total_bytes: u64 = blobs
            .iter()
            .filter_map(|b| b.properties.as_ref()?.content_length)
            .sum();

        if self.config.dry_run {
            for b in &blobs {
                println!("  [dry-run] would download: {}", b.name);
            }
            return Ok(TransferSummary {
                total_files,
                total_bytes,
                succeeded: total_files,
                failed: 0,
                skipped: 0,
            });
        }

        let progress = Arc::new(TransferProgress::new(
            total_files,
            total_bytes,
            self.config.progress,
        ));
        progress.attach_retry_stats(self.client.retry_stats());
        let mut file_tasks: JoinSet<(String, Result<()>)> = JoinSet::new();
        let block_size = self.config.block_size;
        let check_md5 = self.config.check_md5;

        for blob in blobs {
            let relative = blob
                .name
                .strip_prefix(blob_prefix)
                .unwrap_or(&blob.name)
                .trim_start_matches('/')
                .to_string();
            let local_path = local_dir.join(&relative);
            let file_size = blob
                .properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0);
            let expected_md5 = blob
                .properties
                .as_ref()
                .and_then(|p| p.content_md5.clone());

            local::ensure_parent_dir(&local_path).await?;

            let pb = progress.create_file_bar(file_size);
            pb.set_message(relative.clone());

            // File-level admission: bounds concurrent file dispatchers so very
            // large jobs don't spawn 100k tasks at once. Acquired here in the
            // outer loop so back-pressure flows back to listing.
            let file_permit = self.file_semaphore.clone().acquire_owned().await.unwrap();

            let chunk_sem = self.semaphore.clone();
            let client = self.client.clone();
            let progress_c = progress.clone();
            let acct = account.to_string();
            let ctr = container.to_string();
            let blob_name = blob.name.clone();
            let local = local_path.clone();
            let exp = expected_md5.clone();
            let pb_c = pb.clone();

            file_tasks.spawn(async move {
                let _file_permit = file_permit;
                let name_for_err = blob_name.clone();

                let r: Result<()> = async move {
                    if file_size == 0 || file_size < block_size {
                        let result = async {
                            let permit = chunk_sem.acquire_owned().await.unwrap();
                            let data = client.get_blob(&acct, &ctr, &blob_name).await?;
                            drop(permit);
                            if check_md5 {
                                verify_md5(&blob_name, exp.as_deref(), &data)?;
                            }
                            let len = data.len() as u64;
                            tokio::fs::write(&local, &data).await?;
                            pb_c.inc(len);
                            progress_c.add_bytes(len);
                            Ok::<u64, AzcpError>(len)
                        }
                        .await;
                        return match result {
                            Ok(_) => {
                                pb_c.finish_and_clear();
                                progress_c.complete_file();
                                Ok(())
                            }
                            Err(e) => {
                                pb_c.finish_and_clear();
                                Err(e)
                            }
                        };
                    }

                    let f = tokio::fs::File::create(&local).await?;
                    f.set_len(file_size).await?;
                    drop(f);

                    // Open file ONCE per blob and share via Arc<std::fs::File>.
                    // pwrite (write_all_at) is thread-safe at the OS level: many
                    // chunk tasks can write to disjoint offsets concurrently
                    // without a Mutex. Eliminates per-block open/seek/flush
                    // syscalls that were the dominant CPU cost.
                    let shared_file = Arc::new(
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&local)
                            .map_err(|e| AzcpError::Transfer(e.to_string()))?,
                    );

                    let num_chunks = (file_size + block_size - 1) / block_size;
                    let mut block_handles: Vec<tokio::task::JoinHandle<Result<()>>> =
                        Vec::with_capacity(num_chunks as usize);

                    for i in 0..num_chunks {
                        let offset = i * block_size;
                        let length = std::cmp::min(block_size, file_size - offset);

                        let permit = chunk_sem.clone().acquire_owned().await.unwrap();
                        let client_c = client.clone();
                        let acct_c = acct.clone();
                        let ctr_c = ctr.clone();
                        let blob_name_c = blob_name.clone();
                        let file_c = shared_file.clone();
                        let pb_c2 = pb_c.clone();
                        let prog_c2 = progress_c.clone();

                        block_handles.push(tokio::spawn(async move {
                            let _permit = permit;
                            let data = client_c
                                .get_blob_range(&acct_c, &ctr_c, &blob_name_c, offset, length)
                                .await?;
                            let n = data.len() as u64;
                            tokio::task::spawn_blocking(move || pwrite_all(&file_c, &data, offset))
                                .await
                                .map_err(|e| AzcpError::Transfer(e.to_string()))?
                                .map_err(|e| AzcpError::Transfer(e.to_string()))?;
                            pb_c2.inc(n);
                            prog_c2.add_bytes(n);
                            Ok(())
                        }));
                    }

                    let mut block_err: Option<AzcpError> = None;
                    for h in block_handles {
                        match h.await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) if block_err.is_none() => block_err = Some(e),
                            Ok(Err(_)) => {}
                            Err(e) if block_err.is_none() => {
                                block_err = Some(AzcpError::Transfer(e.to_string()))
                            }
                            Err(_) => {}
                        }
                    }
                    if let Some(e) = block_err {
                        pb_c.finish_and_clear();
                        return Err(e);
                    }
                    if check_md5 {
                        if let Err(e) = verify_md5_file(&blob_name, exp.as_deref(), &local).await {
                            pb_c.finish_and_clear();
                            return Err(e);
                        }
                    }
                    pb_c.finish_and_clear();
                    progress_c.complete_file();
                    Ok(())
                }
                .await;
                (name_for_err, r)
            });
        }

        let mut succeeded = 0u64;
        let mut failed = 0u64;

        while let Some(joined) = file_tasks.join_next().await {
            match joined {
                Ok((_, Ok(()))) => {
                    succeeded += 1;
                }
                Ok((name, Err(e))) => {
                    failed += 1;
                    eprintln!("ERROR: {name}: {e}");
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("ERROR: <task join>: {e}");
                }
            }
        }

        progress.finish();

        Ok(TransferSummary {
            total_files,
            total_bytes,
            succeeded,
            failed,
            skipped: 0,
        })
    }
}

// Size-aware partitioning via Longest-Processing-Time (LPT) bin packing.
// Each worker independently re-runs the listing and applies this on the full
// set; determinism (same partition on every worker) requires identical input
// order and tie-breaking, hence the (size desc, name asc) sort and stable
// greedy assignment to the lowest-loaded bin.
fn apply_shard<T, F, S>(items: &mut Vec<T>, shard: Option<(usize, usize)>, name: F, size: S)
where
    F: Fn(&T) -> String,
    S: Fn(&T) -> u64,
{
    let Some((idx, count)) = shard else { return };
    if count <= 1 {
        return;
    }
    let mut indexed: Vec<(usize, u64, String)> = items
        .iter()
        .enumerate()
        .map(|(i, t)| (i, size(t), name(t)))
        .collect();
    // Largest first; ties broken by name for cross-worker determinism.
    indexed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    let mut bin_loads = vec![0u64; count];
    let mut owner = vec![0usize; items.len()];
    for (orig_idx, sz, _) in &indexed {
        // Pick lowest-loaded bin; ties broken by lowest bin index for determinism.
        let bin = bin_loads
            .iter()
            .enumerate()
            .min_by(|(ai, al), (bi, bl)| al.cmp(bl).then_with(|| ai.cmp(bi)))
            .map(|(b, _)| b)
            .unwrap_or(0);
        bin_loads[bin] = bin_loads[bin].saturating_add(*sz);
        owner[*orig_idx] = bin;
    }
    let mut i = 0usize;
    items.retain(|_| {
        let keep = owner[i] == idx;
        i += 1;
        keep
    });
}

pub fn build_glob_set(patterns: Option<&str>) -> Result<Option<GlobSet>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    let patterns = patterns.trim();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let glob = Glob::new(pat)
            .map_err(|e| AzcpError::Transfer(format!("invalid glob `{pat}`: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| AzcpError::Transfer(format!("glob set build failed: {e}")))?;
    Ok(Some(set))
}

fn md5_of(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn verify_md5(blob_path: &str, expected_b64: Option<&str>, data: &[u8]) -> Result<()> {
    let Some(expected) = expected_b64 else {
        return Ok(());
    };
    let actual_b64 = STANDARD.encode(md5_of(data));
    if expected == actual_b64 {
        Ok(())
    } else {
        Err(AzcpError::Md5Mismatch {
            path: blob_path.to_string(),
            expected: expected.to_string(),
            actual: actual_b64,
        })
    }
}

async fn verify_md5_file(
    blob_path: &str,
    expected_b64: Option<&str>,
    local_path: &Path,
) -> Result<()> {
    let Some(expected) = expected_b64 else {
        return Ok(());
    };
    let mut file = tokio::fs::File::open(local_path).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual: [u8; 16] = hasher.finalize().into();
    let actual_b64 = STANDARD.encode(actual);
    if expected == actual_b64 {
        Ok(())
    } else {
        Err(AzcpError::Md5Mismatch {
            path: blob_path.to_string(),
            expected: expected.to_string(),
            actual: actual_b64,
        })
    }
}

fn mime_from_path(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(unix)]
fn pwrite_all(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn pwrite_all(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < buf.len() {
        let n = file.seek_write(&buf[written..], offset + written as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "seek_write returned 0",
            ));
        }
        written += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply_shard;

    #[test]
    fn shard_partitions_disjointly() {
        let names: Vec<String> = ('a'..='p').map(|c| c.to_string()).collect();
        let mut all_seen: Vec<String> = Vec::new();
        for i in 0..4 {
            let mut items = names.clone();
            apply_shard(&mut items, Some((i, 4)), |s| s.clone(), |_| 1);
            assert_eq!(items.len(), 4);
            all_seen.extend(items);
        }
        all_seen.sort();
        assert_eq!(all_seen, names);
    }

    #[test]
    fn shard_none_is_noop() {
        let mut items = vec!["b".to_string(), "a".to_string()];
        apply_shard(&mut items, None, |s| s.clone(), |_| 1);
        assert_eq!(items, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn shard_count_one_is_noop() {
        let mut items = vec!["b".to_string(), "a".to_string()];
        apply_shard(&mut items, Some((0, 1)), |s| s.clone(), |_| 1);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn shard_balances_by_size_lpt() {
        let items: Vec<(String, u64)> = vec![
            ("big1".into(), 100),
            ("a".into(), 1),
            ("b".into(), 1),
            ("c".into(), 1),
            ("d".into(), 1),
        ];
        let mut totals = [0u64; 2];
        for shard in 0..2 {
            let mut s = items.clone();
            apply_shard(&mut s, Some((shard, 2)), |t| t.0.clone(), |t| t.1);
            totals[shard] = s.iter().map(|t| t.1).sum();
        }
        assert_eq!(totals.iter().sum::<u64>(), 104);
        assert_eq!(*totals.iter().max().unwrap(), 100);
        assert_eq!(*totals.iter().min().unwrap(), 4);
    }
}
