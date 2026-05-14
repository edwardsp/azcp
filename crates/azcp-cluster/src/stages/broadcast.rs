use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::ptr::NonNull;
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
use crate::stages::shards::ShardSpec;

const ALIGN: usize = 4096;

struct AlignedBuf {
    ptr: NonNull<u8>,
    cap: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn new(cap: usize) -> Result<Self> {
        let layout = Layout::from_size_align(cap, ALIGN)
            .map_err(|e| anyhow!("invalid bcast buffer layout (cap={cap}): {e}"))?;
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or_else(|| anyhow!("alloc {cap} bytes failed"))?;
        Ok(Self { ptr, cap })
    }
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.cap) }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe {
            let layout = Layout::from_size_align_unchecked(self.cap, ALIGN);
            dealloc(self.ptr.as_ptr(), layout);
        }
    }
}

struct ShardCursor {
    file_idx: usize,
    byte_offset: u64,
    byte_len: u64,
    next_post_offset: u64,
    is_owner: bool,
    reader: Option<File>,
    broadcaster: i32,
}

struct FileState {
    needs_write: bool,
    opened: bool,
    closed: bool,
    size: u64,
    path: PathBuf,
    shards_total: usize,
    shards_posted: usize,
    chunks_in_flight: usize,
    writes_in_flight: usize,
}

struct InFlight {
    request: ffi::MPI_Request,
    slot: usize,
    file_idx: usize,
    n: usize,
    abs_offset: u64,
}

enum WriterCmd {
    OpenFile {
        file_idx: usize,
        file: File,
        size: u64,
    },
    Write {
        file_idx: usize,
        slot: usize,
        n: usize,
        offset: u64,
        buf: AlignedBuf,
    },
    CloseFile {
        file_idx: usize,
    },
}

struct WriteAck {
    slot: usize,
    file_idx: usize,
    buf: AlignedBuf,
    err: Option<anyhow::Error>,
}

pub fn run(
    world: &SimpleCommunicator,
    args: &Args,
    entries: &[BlobItem],
    plan: &[ShardSpec],
    presence: &Presence,
) -> Result<()> {
    let rank = world.rank() as usize;
    let chunk: usize = args.bcast_chunk;
    let depth: usize = args.bcast_pipeline.max(1);
    let num_writers: usize = args.bcast_writers.max(1);
    let direct: bool = !args.no_bcast_direct;

    if direct && chunk % ALIGN != 0 {
        return Err(anyhow!(
            "--bcast-direct requires --bcast-chunk to be a multiple of {ALIGN} (got {chunk})"
        ));
    }

    let mut buffers: Vec<Option<AlignedBuf>> = (0..depth)
        .map(|_| AlignedBuf::new(chunk).map(Some))
        .collect::<Result<_>>()?;
    let mut free_slots: VecDeque<usize> = (0..depth).collect();
    let mut in_flight: Vec<InFlight> = Vec::with_capacity(depth);

    let (ack_tx, ack_rx) = mpsc::channel::<WriteAck>();

    let mut cmd_txs: Vec<mpsc::Sender<WriterCmd>> = Vec::with_capacity(num_writers);
    let mut writer_handles = Vec::with_capacity(num_writers);
    for i in 0..num_writers {
        let (cmd_tx_i, cmd_rx_i) = mpsc::channel::<WriterCmd>();
        let ack_tx_i = ack_tx.clone();
        let h = thread::Builder::new()
            .name(format!("bcast-writer-{i}"))
            .spawn(move || writer_loop(cmd_rx_i, ack_tx_i, direct))
            .context("spawn writer thread")?;
        cmd_txs.push(cmd_tx_i);
        writer_handles.push(h);
    }
    drop(ack_tx);

    let result = drive_pipeline(
        world,
        args,
        entries,
        plan,
        presence,
        rank,
        chunk,
        direct,
        &mut buffers,
        &mut free_slots,
        &mut in_flight,
        &cmd_txs,
        &ack_rx,
    );

    drop(cmd_txs);
    let mut writer_result: Result<()> = Ok(());
    for h in writer_handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if writer_result.is_ok() {
                    writer_result = Err(e);
                }
            }
            Err(_) => {
                if writer_result.is_ok() {
                    writer_result = Err(anyhow!("writer thread panicked"));
                }
            }
        }
    }

    result.and(writer_result)
}

#[allow(clippy::too_many_arguments)]
fn drive_pipeline(
    world: &SimpleCommunicator,
    args: &Args,
    entries: &[BlobItem],
    plan: &[ShardSpec],
    presence: &Presence,
    rank: usize,
    chunk: usize,
    direct: bool,
    buffers: &mut [Option<AlignedBuf>],
    free_slots: &mut VecDeque<usize>,
    in_flight: &mut Vec<InFlight>,
    cmd_txs: &[mpsc::Sender<WriterCmd>],
    ack_rx: &mpsc::Receiver<WriteAck>,
) -> Result<()> {
    // Pre-aggregate per-file state from the shard plan. Multiple shards
    // of the same file collapse to one FileState (writer key, OpenFile/
    // CloseFile dedup). The local file is expected to already exist at
    // its full size (main.rs ftruncates every file this rank touches
    // before invoking bcast); broadcast.rs only opens for write.
    let mut file_state: HashMap<usize, FileState> = HashMap::new();
    for s in plan {
        let entry = &entries[s.file_idx];
        let size = entry
            .properties
            .as_ref()
            .and_then(|p| p.content_length)
            .unwrap_or(0);
        let path = args.dest.join(local_rel(&args.source, &entry.name));
        let fs = file_state
            .entry(s.file_idx)
            .or_insert_with(|| FileState {
                needs_write: false,
                opened: false,
                closed: false,
                size,
                path,
                shards_total: 0,
                shards_posted: 0,
                chunks_in_flight: 0,
                writes_in_flight: 0,
            });
        fs.shards_total += 1;
    }
    // needs_write iff this rank doesn't already have the file AND doesn't
    // own every shard of it (i.e. at least one shard arrives over bcast).
    for (&file_idx, fs) in file_state.iter_mut() {
        let already_have = presence.has(rank, file_idx);
        let owns_all = plan
            .iter()
            .filter(|s| s.file_idx == file_idx)
            .all(|s| s.owner_rank == rank);
        fs.needs_write = !already_have && !owns_all && fs.size > 0;
    }

    let mut next_plan_idx: usize = 0;
    let mut active: Option<ShardCursor> = None;
    let mut writes_pending: usize = 0;

    // MPI_Ibcast count is a C int; cap each call at < i32::MAX. See v0.3.x
    // for the original analysis.
    const MAX_SUB_BCAST: usize = 1 << 30;
    let mut slot_subs_remaining: Vec<usize> = vec![0; buffers.len()];

    loop {
        while !free_slots.is_empty() {
            if active.is_none() {
                if next_plan_idx >= plan.len() {
                    break;
                }
                let s = &plan[next_plan_idx];
                next_plan_idx += 1;

                let file_idx = s.file_idx;
                let is_owner = s.owner_rank == rank;
                let broadcaster = s.owner_rank as i32;

                let fs = file_state.get(&file_idx).unwrap();
                let path = fs.path.clone();
                let size = fs.size;
                let needs_write = fs.needs_write;
                let already_opened = fs.opened;

                let reader = if is_owner && s.byte_len > 0 {
                    let mut r = File::open(&path)
                        .with_context(|| format!("owner open {}", path.display()))?;
                    if s.byte_offset > 0 {
                        use std::io::{Seek, SeekFrom};
                        r.seek(SeekFrom::Start(s.byte_offset)).with_context(|| {
                            format!(
                                "owner seek {} to byte_offset={}",
                                path.display(),
                                s.byte_offset
                            )
                        })?;
                    }
                    Some(r)
                } else {
                    None
                };

                if needs_write && !already_opened {
                    let mut opts = OpenOptions::new();
                    opts.write(true);
                    if direct {
                        opts.custom_flags(libc::O_DIRECT);
                    }
                    let w = opts
                        .open(&path)
                        .with_context(|| format!("receiver open {}", path.display()))?;
                    cmd_txs[file_idx % cmd_txs.len()]
                        .send(WriterCmd::OpenFile {
                            file_idx,
                            file: w,
                            size,
                        })
                        .map_err(|_| anyhow!("writer thread closed"))?;
                    file_state.get_mut(&file_idx).unwrap().opened = true;
                }

                if s.byte_len == 0 {
                    let fs = file_state.get_mut(&file_idx).unwrap();
                    fs.shards_posted += 1;
                    maybe_close_file(&mut file_state, file_idx, cmd_txs)?;
                    continue;
                }

                active = Some(ShardCursor {
                    file_idx,
                    byte_offset: s.byte_offset,
                    byte_len: s.byte_len,
                    next_post_offset: 0,
                    is_owner,
                    reader,
                    broadcaster,
                });
            }

            let cur = active.as_mut().unwrap();
            let remaining = cur.byte_len - cur.next_post_offset;
            let n = remaining.min(chunk as u64) as usize;
            let within_shard_offset = cur.next_post_offset;
            let abs_offset = cur.byte_offset + within_shard_offset;
            let slot = free_slots.pop_front().unwrap();
            let buf = buffers[slot].as_mut().expect("free slot has buffer");

            if cur.is_owner {
                let path_for_err = file_state[&cur.file_idx].path.clone();
                cur.reader
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut buf.as_mut_slice()[..n])
                    .with_context(|| {
                        format!("owner read {} bytes from {}", n, path_for_err.display())
                    })?;
            }

            let mut req: ffi::MPI_Request = unsafe { std::mem::zeroed() };
            let buf_ptr = buf.as_mut_ptr();
            let mut sub_offset: usize = 0;
            while sub_offset < n {
                let sub = (n - sub_offset).min(MAX_SUB_BCAST);
                let rc = unsafe {
                    ffi::MPI_Ibcast(
                        buf_ptr.add(sub_offset) as *mut std::ffi::c_void,
                        sub as i32,
                        ffi::RSMPI_UINT8_T,
                        cur.broadcaster,
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
                    file_idx: cur.file_idx,
                    n,
                    abs_offset,
                });
                slot_subs_remaining[slot] += 1;
                sub_offset += sub;
            }

            file_state.get_mut(&cur.file_idx).unwrap().chunks_in_flight += 1;
            cur.next_post_offset += n as u64;

            if cur.next_post_offset >= cur.byte_len {
                let fid = cur.file_idx;
                file_state.get_mut(&fid).unwrap().shards_posted += 1;
                active = None;
                maybe_close_file(&mut file_state, fid, cmd_txs)?;
            }
        }

        loop {
            match ack_rx.try_recv() {
                Ok(ack) => {
                    if let Some(e) = ack.err {
                        return Err(e);
                    }
                    buffers[ack.slot] = Some(ack.buf);
                    free_slots.push_back(ack.slot);
                    writes_pending -= 1;
                    let fid = ack.file_idx;
                    file_state.get_mut(&fid).unwrap().writes_in_flight -= 1;
                    maybe_close_file(&mut file_state, fid, cmd_txs)?;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("writer thread closed unexpectedly"));
                }
            }
        }

        if in_flight.is_empty()
            && writes_pending == 0
            && next_plan_idx >= plan.len()
            && active.is_none()
        {
            break;
        }

        if in_flight.is_empty() {
            let ack = ack_rx
                .recv()
                .map_err(|_| anyhow!("writer thread closed unexpectedly"))?;
            if let Some(e) = ack.err {
                return Err(e);
            }
            buffers[ack.slot] = Some(ack.buf);
            free_slots.push_back(ack.slot);
            writes_pending -= 1;
            let fid = ack.file_idx;
            file_state.get_mut(&fid).unwrap().writes_in_flight -= 1;
            maybe_close_file(&mut file_state, fid, cmd_txs)?;
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
        slot_subs_remaining[done.slot] -= 1;
        if slot_subs_remaining[done.slot] > 0 {
            continue;
        }

        let fs = file_state.get_mut(&done.file_idx).unwrap();
        fs.chunks_in_flight -= 1;
        let needs_write = fs.needs_write;

        if needs_write {
            let buf = buffers[done.slot]
                .take()
                .expect("slot must hold buffer at completion");
            cmd_txs[done.file_idx % cmd_txs.len()]
                .send(WriterCmd::Write {
                    file_idx: done.file_idx,
                    slot: done.slot,
                    n: done.n,
                    offset: done.abs_offset,
                    buf,
                })
                .map_err(|_| anyhow!("writer thread closed"))?;
            writes_pending += 1;
            file_state.get_mut(&done.file_idx).unwrap().writes_in_flight += 1;
        } else {
            free_slots.push_back(done.slot);
        }

        maybe_close_file(&mut file_state, done.file_idx, cmd_txs)?;
    }

    Ok(())
}

fn maybe_close_file(
    file_state: &mut HashMap<usize, FileState>,
    file_idx: usize,
    cmd_txs: &[mpsc::Sender<WriterCmd>],
) -> Result<()> {
    let fs = file_state.get_mut(&file_idx).unwrap();
    if !fs.opened
        || fs.closed
        || fs.shards_posted != fs.shards_total
        || fs.chunks_in_flight != 0
        || fs.writes_in_flight != 0
    {
        return Ok(());
    }
    cmd_txs[file_idx % cmd_txs.len()]
        .send(WriterCmd::CloseFile { file_idx })
        .map_err(|_| anyhow!("writer thread closed"))?;
    fs.closed = true;
    Ok(())
}

fn writer_loop(
    cmd_rx: mpsc::Receiver<WriterCmd>,
    ack_tx: mpsc::Sender<WriteAck>,
    direct: bool,
) -> Result<()> {
    let mut files: HashMap<usize, (File, u64)> = HashMap::new();
    let mut deferred_err: Option<anyhow::Error> = None;
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WriterCmd::OpenFile {
                file_idx,
                file,
                size,
            } => {
                files.insert(file_idx, (file, size));
            }
            WriterCmd::Write {
                file_idx,
                slot,
                n,
                offset,
                buf,
            } => {
                let err = match files.get(&file_idx) {
                    Some((w, _size)) => {
                        // O_DIRECT requires aligned write length; round up,
                        // padding bytes from buffer's prior contents land past
                        // the shard boundary. The CLI enforces
                        // `shard_size % bcast_chunk == 0` so non-final shards
                        // never have a short trailing write — only the file's
                        // very last chunk does, and CloseFile's ftruncate
                        // trims it.
                        let write_len = if direct {
                            (n + ALIGN - 1) & !(ALIGN - 1)
                        } else {
                            n
                        };
                        let write_len = write_len.min(buf.cap);
                        w.write_all_at(&buf.as_slice()[..write_len], offset)
                            .err()
                            .map(|e| anyhow!("write file_idx={file_idx} offset={offset}: {e}"))
                    }
                    None => Some(anyhow!("write to unknown file_idx={file_idx}")),
                };
                if ack_tx
                    .send(WriteAck {
                        slot,
                        file_idx,
                        buf,
                        err,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            WriterCmd::CloseFile { file_idx } => {
                if let Some((f, size)) = files.remove(&file_idx) {
                    if direct {
                        if let Err(e) = f.set_len(size) {
                            if deferred_err.is_none() {
                                deferred_err =
                                    Some(anyhow!("ftruncate file_idx={file_idx}: {e}"));
                            }
                            continue;
                        }
                    }
                    if let Err(e) = f.sync_all() {
                        if deferred_err.is_none() {
                            deferred_err = Some(anyhow!("close file_idx={file_idx}: {e}"));
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
