use std::future::Future;

use crate::error::{AzcpError, Result};
use crate::storage::blob::models::BlobInfo;

#[derive(Debug)]
pub(super) enum DownloadSource {
    Directory(String),
    Blob(BlobInfo),
}

pub(super) async fn resolve_source(
    path: &str,
    head: impl Future<Output = Result<BlobInfo>>,
    recursive: bool,
) -> Result<DownloadSource> {
    if path.is_empty() || path.ends_with('/') {
        return Ok(DownloadSource::Directory(path.to_owned()));
    }
    match head.await {
        Ok(props) => Ok(DownloadSource::Blob(props)),
        Err(AzcpError::Storage { status: 404, .. }) if recursive => {
            Ok(DownloadSource::Directory(format!("{path}/")))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
