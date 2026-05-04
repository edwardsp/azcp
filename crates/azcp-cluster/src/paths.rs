use azcp::{parse_location, Location};

/// Compute the local-relative path for a blob `name` by stripping the source
/// URL's blob-path prefix. Mirrors `engine::download_entries` so the download,
/// diff, and broadcast stages all agree on where a file lives on disk.
pub fn local_rel(source: &str, name: &str) -> String {
    let prefix = match parse_location(source) {
        Ok(Location::AzureBlob(b)) => b.path,
        _ => String::new(),
    };
    let stripped = if prefix.is_empty() {
        name
    } else {
        name.strip_prefix(&prefix).unwrap_or(name)
    };
    stripped.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_path_prefix() {
        let s = "https://acct.blob.core.windows.net/ctr/models/llama/";
        assert_eq!(local_rel(s, "models/llama/file.bin"), "file.bin");
        assert_eq!(
            local_rel(s, "models/llama/.cache/h/.gitignore"),
            ".cache/h/.gitignore"
        );
    }

    #[test]
    fn no_prefix_returns_name_unchanged() {
        let s = "https://acct.blob.core.windows.net/ctr/";
        assert_eq!(local_rel(s, "file.bin"), "file.bin");
        assert_eq!(local_rel(s, "/file.bin"), "file.bin");
    }

    #[test]
    fn name_not_under_prefix_falls_back() {
        let s = "https://acct.blob.core.windows.net/ctr/prefix/";
        assert_eq!(local_rel(s, "other/file.bin"), "other/file.bin");
    }

    #[test]
    fn local_source_yields_raw_trimmed_name() {
        assert_eq!(local_rel("/some/local/path", "a/b.bin"), "a/b.bin");
        assert_eq!(local_rel("/some/local/path", "/a/b.bin"), "a/b.bin");
    }
}
