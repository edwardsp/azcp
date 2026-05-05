use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;

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
    remaining_to_post: u64,
    next_post_offset: u64,
    chunks_in_flight: usize,
    is_owner: bool,
    needs_write: bool,
    path: PathBuf,
}

struct InFlight {
    request: ffi::MPI_Request,
    slot: usize,
    file_id: usize,
    n: usize,
    offset: u64,
}

enum WriterCmd {
    OpenFile {
        file_id: usize,
        file: File,
    },
    Write {
        file_id: usize,
        slot: usize,
        n: usize,
        offset: u64,
        buf: Vec<u8>,
    },
    CloseFile {
        file_id: usize,
    },
}

struct WriteAck {
    slot: usize,
    buf: Vec<u8>,
    err: Option<anyhow::Error>,
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

    let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCmd>();
    let (ack_tx, ack_rx) = mpsc::channel::<WriteAck>();

    let writer = thread::Builder::new()
        .name("bcast-writer".into())
        .spawn(move || writer_loop(cmd_rx, ack_tx))
        .context("spawn writer thread")?;

    let result = drive_pipeline(
        world,
        args,
        entries,
        plan,
        presence,
        rank,
        chunk,
        &mut buffers,
        &mut free_slots,
        &mut in_flight,
        &cmd_tx,
        &ack_rx,
    );

    drop(cmd_tx);
    let writer_result = writer
        .join()
        .map_err(|_| anyhow!("writer thread panicked"))?;

    result.and(writer_result)
}

#[allow(clippy::too_many_arguments)]
fn drive_pipeline(
    world: &SimpleCommunicator,
    args: &Args,
    entries: &[BlobItem],
    plan: &[(usize, usize)],
    presence: &Presence,
    rank: usize,
    chunk: usize,
    buffers: &mut [Vec<u8>],
    free_slots: &mut VecDeque<usize>,
    in_flight: &mut Vec<InFlight>,
    cmd_tx: &mpsc::Sender<WriterCmd>,
    ack_rx: &mpsc::Receiver<WriteAck>,
) -> Result<()> {
    let mut files: Vec<Option<FileCtx>> = Vec::with_capacity(plan.len());
    let mut next_file_idx: usize = 0;
    let mut current_file: Option<usize> = None;
    let mut current_broadcaster: i32 = 0;
    let mut writes_pending: usize = 0;

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

                let (reader, needs_write) = if is_owner {
                    if size == 0 {
                        (None, false)
                    } else {
                        let r = File::open(&local_path)
                            .with_context(|| format!("owner open {}", local_path.display()))?;
                        (Some(r), false)
                    }
                } else if !already_have {
                    ensure_parent(&local_path)?;
                    let w = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&local_path)
                        .with_context(|| format!("receiver create {}", local_path.display()))?;
                    if size == 0 {
                        // Zero-byte file: created and truncated; no chunks will flow.
                        // Drop on this thread to release the fd immediately.
                        drop(w);
                        (None, false)
                    } else {
                        cmd_tx
                            .send(WriterCmd::OpenFile {
                                file_id: files.len(),
                                file: w,
                            })
                            .map_err(|_| anyhow!("writer thread closed"))?;
                        (None, true)
                    }
                } else {
                    (None, false)
                };

                let ctx = FileCtx {
                    reader,
                    remaining_to_post: size,
                    next_post_offset: 0,
                    chunks_in_flight: 0,
                    is_owner,
                    needs_write,
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
            let offset = ctx.next_post_offset;
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
                offset,
            });
            ctx.chunks_in_flight += 1;
            ctx.remaining_to_post -= n as u64;
            ctx.next_post_offset += n as u64;

            if ctx.remaining_to_post == 0 {
                current_file = None;
            }
        }

        loop {
            match ack_rx.try_recv() {
                Ok(ack) => {
                    if let Some(e) = ack.err {
                        return Err(e);
                    }
                    buffers[ack.slot] = ack.buf;
                    free_slots.push_back(ack.slot);
                    writes_pending -= 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("writer thread closed unexpectedly"));
                }
            }
        }

        if in_flight.is_empty() && writes_pending == 0 && next_file_idx >= plan.len() {
            break;
        }

        if in_flight.is_empty() {
            let ack = ack_rx
                .recv()
                .map_err(|_| anyhow!("writer thread closed unexpectedly"))?;
            if let Some(e) = ack.err {
                return Err(e);
            }
            buffers[ack.slot] = ack.buf;
            free_slots.push_back(ack.slot);
            writes_pending -= 1;
            continue;
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

        let file_finished = ctx.chunks_in_flight == 0 && ctx.remaining_to_post == 0;

        if ctx.needs_write {
            // Hand the buffer to the writer; replace with an empty Vec so the
            // slot is reserved until the writer returns it.
            let buf = std::mem::take(&mut buffers[done.slot]);
            cmd_tx
                .send(WriterCmd::Write {
                    file_id: done.file_id,
                    slot: done.slot,
                    n: done.n,
                    offset: done.offset,
                    buf,
                })
                .map_err(|_| anyhow!("writer thread closed"))?;
            writes_pending += 1;
        } else {
            free_slots.push_back(done.slot);
        }

        if file_finished {
            if ctx.needs_write {
                cmd_tx
                    .send(WriterCmd::CloseFile {
                        file_id: done.file_id,
                    })
                    .map_err(|_| anyhow!("writer thread closed"))?;
            }
            files[done.file_id] = None;
        }
    }

    Ok(())
}

fn writer_loop(cmd_rx: mpsc::Receiver<WriterCmd>, ack_tx: mpsc::Sender<WriteAck>) -> Result<()> {
    let mut files: HashMap<usize, File> = HashMap::new();
    let mut deferred_err: Option<anyhow::Error> = None;
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WriterCmd::OpenFile { file_id, file } => {
                files.insert(file_id, file);
            }
            WriterCmd::Write {
                file_id,
                slot,
                n,
                offset,
                buf,
            } => {
                let err = match files.get(&file_id) {
                    Some(w) => w
                        .write_all_at(&buf[..n], offset)
                        .err()
                        .map(|e| anyhow!("write file_id={file_id} offset={offset}: {e}")),
                    None => Some(anyhow!("write to unknown file_id={file_id}")),
                };
                if ack_tx.send(WriteAck { slot, buf, err }).is_err() {
                    return Ok(());
                }
            }
            WriterCmd::CloseFile { file_id } => {
                if let Some(f) = files.remove(&file_id) {
                    if let Err(e) = f.sync_all() {
                        if deferred_err.is_none() {
                            deferred_err = Some(anyhow!("close file_id={file_id}: {e}"));
                        }
                    }
                }
            }
        }
    }
    match deferred_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("mkdir -p {}: {e}", parent.display()))?;
    }
    Ok(())
}
