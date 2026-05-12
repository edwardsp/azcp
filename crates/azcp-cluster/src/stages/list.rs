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
    let skipped = before - entries.len();
    if skipped > 0 && rank == ROOT_RANK {
        eprintln!(
            "[list] skipped {skipped} directory-marker blob(s) (name == source prefix or ends in '/')"
        );
    }
    Ok(entries)
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
