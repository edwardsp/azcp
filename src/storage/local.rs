use std::path::{Path, PathBuf};
use tokio::fs;

use crate::error::Result;

#[derive(Clone)]
pub struct LocalEntry {
    pub path: PathBuf,
    pub relative_path: String,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

pub async fn walk_directory(base: &Path, recursive: bool) -> Result<Vec<LocalEntry>> {
    let mut entries = Vec::new();
    walk_inner(base, base, recursive, &mut entries).await?;
    Ok(entries)
}

fn walk_inner<'a>(
    base: &'a Path,
    current: &'a Path,
    recursive: bool,
    entries: &'a mut Vec<LocalEntry>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut reader = fs::read_dir(current).await?;
        while let Some(entry) = reader.next_entry().await? {
            let path = entry.path();
            let metadata = entry.metadata().await?;

            if metadata.is_file() {
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");

                entries.push(LocalEntry {
                    path: path.clone(),
                    relative_path: relative,
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                });
            } else if metadata.is_dir() && recursive {
                walk_inner(base, &path, true, entries).await?;
            }
        }
        Ok(())
    })
}

pub async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).await?;
        }
    }
    Ok(())
}
