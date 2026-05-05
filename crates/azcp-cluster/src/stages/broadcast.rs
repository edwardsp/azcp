use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use mpi::ffi;
use mpi::topology::SimpleCommunicator;
use mpi::traits::*;

use azcp::BlobItem;

use crate::cli::Args;
use crate::paths::local_rel;
use crate::stages::presence::Presence;

struct FileCtx {
    reader: Option<File>,
    writer: Option<File>,
    remaining_to_post: u64,
    chunks_in_flight: usize,
    is_owner: bool,
    path: std::path::PathBuf,
}

struct InFlight {
    request: ffi::MPI_Request,
    slot: usize,
    file_id: usize,
    n: usize,
}

pub fn run(
    world: &SimpleCommunicator,
    args: &Args,
    entries: &[BlobItem],
    plan: &[(usize, usize)],
    presence: &Presence,
) -> Result<()> {
    let rank = world.rank() as usize;
    let chunk: usize = args.bcast_chunk;
    let depth: usize = args.bcast_pipeline.max(1);

    let mut buffers: Vec<Vec<u8>> = (0..depth).map(|_| vec![0u8; chunk]).collect();
    let mut free_slots: VecDeque<usize> = (0..depth).collect();
    let mut in_flight: Vec<InFlight> = Vec::with_capacity(depth);

    let mut files: Vec<Option<FileCtx>> = Vec::with_capacity(plan.len());
    let mut next_file_idx: usize = 0;
    let mut current_file: Option<usize> = None;
    let mut current_broadcaster: i32 = 0;

    loop {
        while !free_slots.is_empty() {
            if current_file.is_none() {
                if next_file_idx >= plan.len() {
                    break;
                }
                let (file_idx, broadcaster) = plan[next_file_idx];
                next_file_idx += 1;

                let entry = &entries[file_idx];
                let size = entry
                    .properties
                    .as_ref()
                    .and_then(|p| p.content_length)
                    .unwrap_or(0);
                let local_path = args.dest.join(local_rel(&args.source, &entry.name));

                let is_owner = broadcaster == rank;
                let already_have = presence.has(rank, file_idx);

                let (reader, writer) = if is_owner {
                    let r = File::open(&local_path)
                        .with_context(|| format!("owner open {}", local_path.display()))?;
                    (Some(r), None)
                } else if !already_have {
                    ensure_parent(&local_path)?;
                    let w = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&local_path)
                        .with_context(|| format!("receiver create {}", local_path.display()))?;
                    (None, Some(w))
                } else {
                    (None, None)
                };

                let ctx = FileCtx {
                    reader,
                    writer,
                    remaining_to_post: size,
                    chunks_in_flight: 0,
                    is_owner,
                    path: local_path,
                };
                files.push(Some(ctx));
                current_file = Some(files.len() - 1);
                current_broadcaster = broadcaster as i32;
            }

            let fid = current_file.unwrap();
            let ctx = files[fid].as_mut().unwrap();
            if ctx.remaining_to_post == 0 {
                current_file = None;
                continue;
            }

            let n = ctx.remaining_to_post.min(chunk as u64) as usize;
            let slot = free_slots.pop_front().unwrap();
            let buf = &mut buffers[slot];

            if ctx.is_owner {
                ctx.reader
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut buf[..n])
                    .with_context(|| {
                        format!("owner read {} bytes from {}", n, ctx.path.display())
                    })?;
            }

            let mut req: ffi::MPI_Request = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                ffi::MPI_Ibcast(
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    n as i32,
                    ffi::RSMPI_UINT8_T,
                    current_broadcaster,
                    world.as_raw(),
                    &mut req,
                )
            };
            if rc != ffi::MPI_SUCCESS as i32 {
                return Err(anyhow!("MPI_Ibcast failed with code {rc}"));
            }

            in_flight.push(InFlight {
                request: req,
                slot,
                file_id: fid,
                n,
            });
            ctx.chunks_in_flight += 1;
            ctx.remaining_to_post -= n as u64;

            if ctx.remaining_to_post == 0 {
                current_file = None;
            }
        }

        if in_flight.is_empty() {
            break;
        }

        let mut raw_reqs: Vec<ffi::MPI_Request> = in_flight.iter().map(|f| f.request).collect();
        let mut completed_idx: i32 = 0;
        let rc = unsafe {
            ffi::MPI_Waitany(
                raw_reqs.len() as i32,
                raw_reqs.as_mut_ptr(),
                &mut completed_idx,
                ffi::RSMPI_STATUS_IGNORE,
            )
        };
        if rc != ffi::MPI_SUCCESS as i32 {
            return Err(anyhow!("MPI_Waitany failed with code {rc}"));
        }

        let done = in_flight.swap_remove(completed_idx as usize);
        let ctx = files[done.file_id].as_mut().unwrap();
        ctx.chunks_in_flight -= 1;
        if let Some(w) = ctx.writer.as_mut() {
            w.write_all(&buffers[done.slot][..done.n])
                .with_context(|| format!("write {}", ctx.path.display()))?;
        }
        free_slots.push_back(done.slot);

        if ctx.chunks_in_flight == 0 && ctx.remaining_to_post == 0 {
            if let Some(mut w) = ctx.writer.take() {
                w.flush().ok();
            }
            files[done.file_id] = None;
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
