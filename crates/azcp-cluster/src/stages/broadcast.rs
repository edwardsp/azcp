use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use mpi::topology::SimpleCommunicator;
use mpi::traits::*;

use azcp::BlobItem;

use crate::cli::Args;
use crate::paths::local_rel;
use crate::stages::presence::Presence;

pub fn run(
    world: &SimpleCommunicator,
    args: &Args,
    entries: &[BlobItem],
    plan: &[(usize, usize)],
    presence: &Presence,
) -> Result<()> {
    let rank = world.rank() as usize;
    let chunk: usize = args.bcast_chunk;
    let mut buf = vec![0u8; chunk];

    for &(file_idx, broadcaster) in plan {
        let entry = &entries[file_idx];
        let size = entry
            .properties
            .as_ref()
            .and_then(|p| p.content_length)
            .unwrap_or(0);
        let local_path = args.dest.join(local_rel(&args.source, &entry.name));

        let is_owner = broadcaster == rank;
        let already_have_locally = presence.has(rank, file_idx);

        let mut owner_reader: Option<File> = None;
        let mut writer: Option<File> = None;
        if is_owner {
            owner_reader = Some(
                File::open(&local_path)
                    .with_context(|| format!("owner open {}", local_path.display()))?,
            );
        } else if !already_have_locally {
            ensure_parent(&local_path)?;
            writer = Some(
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&local_path)
                    .with_context(|| format!("receiver create {}", local_path.display()))?,
            );
        }

        let mut remaining = size;
        while remaining > 0 {
            let n = remaining.min(chunk as u64) as usize;
            if is_owner {
                owner_reader
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut buf[..n])
                    .with_context(|| {
                        format!("owner read {} bytes from {}", n, local_path.display())
                    })?;
            }
            world
                .process_at_rank(broadcaster as i32)
                .broadcast_into(&mut buf[..n]);
            if let Some(w) = writer.as_mut() {
                w.write_all(&buf[..n])
                    .with_context(|| format!("write {}", local_path.display()))?;
            }
            remaining -= n as u64;
        }

        if let Some(mut w) = writer.take() {
            w.flush().ok();
        }
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("mkdir -p {}: {e}", parent.display()))?;
    }
    Ok(())
}
