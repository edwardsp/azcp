use anyhow::{anyhow, Context, Result};
use mpi::topology::SimpleCommunicator;
use mpi::traits::*;

use azcp::{parse_location, BlobClient, BlobItem, Credential, Location};

use crate::cli::Args;
use crate::filelist;
use crate::paths::is_directory_marker;

const ROOT_RANK: i32 = 0;

pub fn run(world: &SimpleCommunicator, args: &Args) -> Result<Vec<BlobItem>> {
    let rank = world.rank();

    let payload: Vec<u8> = if rank == ROOT_RANK {
        let entries = produce_entries(args)?;
        filelist::serialize(&entries).into_bytes()
    } else {
        Vec::new()
    };

    let mut len: i64 = payload.len() as i64;
    world.process_at_rank(ROOT_RANK).broadcast_into(&mut len);

    let mut buf = if rank == ROOT_RANK {
        payload
    } else {
        vec![0u8; len as usize]
    };
    world
        .process_at_rank(ROOT_RANK)
        .broadcast_into(&mut buf[..]);

    let text =
        std::str::from_utf8(&buf).context("filelist payload from rank 0 is not valid UTF-8")?;
    let entries = filelist::deserialize(text);
    let before = entries.len();
    let entries: Vec<BlobItem> = entries
        .into_iter()
        .filter(|e| !is_directory_marker(&args.source, &e.name))
        .collect();
    let after_marker = entries.len();
    let entries = drop_implicit_directories(entries);
    let after_implicit = entries.len();
    let skipped_markers = before - after_marker;
    let skipped_implicit = after_marker - after_implicit;
    if rank == ROOT_RANK {
        if skipped_markers > 0 {
            eprintln!(
                "[list] skipped {skipped_markers} directory-marker blob(s) (name == source prefix or ends in '/')"
            );
        }
        if skipped_implicit > 0 {
            eprintln!(
                "[list] skipped {skipped_implicit} implicit-directory blob(s) (name is a parent prefix of another blob)"
            );
        }
    }
    Ok(entries)
}

/// Filter out zero-byte ADLS Gen2 "implicit directory" blobs whose name
/// equals a parent path of another blob in the listing. Without this, the
/// downstream stage tries to `create(2)` a file at a path where another
/// blob has forced a directory to exist, failing with EISDIR (os error 21).
fn drop_implicit_directories(entries: Vec<BlobItem>) -> Vec<BlobItem> {
    use std::collections::HashSet;
    let mut parents: HashSet<String> = HashSet::new();
    for e in &entries {
        let name = e.name.as_str();
        let mut idx = 0usize;
        while let Some(off) = name[idx..].find('/') {
            let end = idx + off;
            parents.insert(name[..end].to_string());
            idx = end + 1;
        }
    }
    entries
        .into_iter()
        .filter(|e| !parents.contains(&e.name))
        .collect()
}

fn produce_entries(args: &Args) -> Result<Vec<BlobItem>> {
    if let Some(path) = &args.shardlist {
        return azcp::read_shardlist(path)
            .map_err(|e| anyhow!("failed to read --shardlist {}: {e}", path.display()));
    }

    let loc = parse_location(&args.source)
        .map_err(|e| anyhow!("failed to parse source {}: {e}", args.source))?;

    let blob = match loc {
        Location::AzureBlob(b) => b,
        Location::Local(p) => {
            return Err(anyhow!(
                "source must be an Azure Blob URL for cluster broadcast (got local path {p})"
            ));
        }
    };

    let credential = Credential::resolve_or_anonymous(&blob.account, blob.sas_token.as_deref())
        .map_err(|e| anyhow!("failed to resolve credentials for {}: {e}", blob.account))?;
    let client =
        BlobClient::new(credential).map_err(|e| anyhow!("failed to construct BlobClient: {e}"))?;

    let prefix = if blob.path.is_empty() {
        None
    } else {
        Some(blob.path.as_str())
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for LIST")?;

    let entries = rt
        .block_on(client.list_blobs(&blob.account, &blob.container, prefix, true))
        .map_err(|e| anyhow!("LIST failed: {e}"))?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bi(name: &str) -> BlobItem {
        BlobItem {
            name: name.into(),
            properties: None,
        }
    }

    #[test]
    fn drops_blob_whose_name_is_parent_of_another() {
        let entries = vec![
            bi("step/model_ema"),
            bi("step/model_ema/file.bin"),
            bi("step/model_ema/sub/leaf.bin"),
            bi("step/model"),
            bi("step/model/weights.bin"),
            bi("step/keep.txt"),
        ];
        let names: Vec<String> = drop_implicit_directories(entries)
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "step/model_ema/file.bin",
                "step/model_ema/sub/leaf.bin",
                "step/model/weights.bin",
                "step/keep.txt",
            ]
        );
    }

    #[test]
    fn keeps_all_when_no_parent_collisions() {
        let entries = vec![bi("a/b.bin"), bi("a/c.bin"), bi("d.bin")];
        let kept = drop_implicit_directories(entries);
        assert_eq!(kept.len(), 3);
    }
}
