use super::{resolve_source, DownloadSource};
use crate::error::AzcpError;
use crate::storage::blob::models::BlobInfo;

fn head_error(status: u16) -> AzcpError {
    AzcpError::Storage {
        status,
        message: "HEAD dataset failed".into(),
    }
}

#[tokio::test]
async fn recursive_missing_blob_resolves_to_slash_delimited_prefix() {
    let head = async { Err(head_error(404)) };

    let result = resolve_source("dataset", head, true).await;

    assert!(matches!(result, Ok(DownloadSource::Directory(prefix)) if prefix == "dataset/"));
}

#[tokio::test]
async fn recursive_nested_prefix_preserves_path() {
    let head = async { Err(head_error(404)) };

    let result = resolve_source("models/checkpoint.v1", head, true).await;

    assert!(
        matches!(result, Ok(DownloadSource::Directory(prefix)) if prefix == "models/checkpoint.v1/")
    );
}

#[tokio::test]
async fn explicit_directory_skips_head_with_or_without_recursion() {
    for recursive in [false, true] {
        for path in ["", "dataset/"] {
            let head = async { panic!("directory must not issue HEAD") };

            let result = resolve_source(path, head, recursive).await;

            assert!(matches!(result, Ok(DownloadSource::Directory(prefix)) if prefix == path));
        }
    }
}

#[tokio::test]
async fn existing_blob_wins_even_with_recursion() {
    for recursive in [false, true] {
        let props = BlobInfo {
            name: "dataset".into(),
            content_length: 123,
            content_type: "application/octet-stream".into(),
            last_modified: None,
            etag: None,
            content_md5: Some("checksum".into()),
        };

        let result = resolve_source("dataset", async { Ok(props) }, recursive).await;

        match result.unwrap() {
            DownloadSource::Blob(props) => {
                assert_eq!(props.name, "dataset");
                assert_eq!(props.content_length, 123);
                assert_eq!(props.content_md5.as_deref(), Some("checksum"));
            }
            DownloadSource::Directory(_) => panic!("existing blob must remain a file"),
        }
    }
}

#[tokio::test]
async fn nonrecursive_missing_blob_preserves_404() {
    let head = async { Err(head_error(404)) };

    let result = resolve_source("dataset", head, false).await;

    assert!(
        matches!(result, Err(AzcpError::Storage { status: 404, message }) if message == "HEAD dataset failed")
    );
}

#[tokio::test]
async fn recursive_other_storage_errors_are_not_hidden_by_listing() {
    for expected in [400, 401, 403, 429, 500, 503] {
        let head = async { Err(head_error(expected)) };

        let result = resolve_source("dataset", head, true).await;

        assert!(
            matches!(result, Err(AzcpError::Storage { status, message }) if status == expected && message == "HEAD dataset failed")
        );
    }
}

#[tokio::test]
async fn recursive_transport_failure_is_not_hidden_by_listing() {
    let head = async {
        Err(AzcpError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connection timed out",
        )))
    };

    let result = resolve_source("dataset", head, true).await;

    assert!(
        matches!(result, Err(AzcpError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
    );
}
