mod cli;
mod filelist;
mod paths;
mod stages;
mod timing;

use std::process::ExitCode;

use clap::Parser;
use mpi::collective::CommunicatorCollectives;
use mpi::environment;
use mpi::ffi;
use mpi::traits::*;

use azcp::{BlobItem, DownloadRange};

use cli::{Args, Stage};

fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(e) = args.validate() {
        eprintln!("[fatal] invalid arguments: {e}");
        return ExitCode::from(2);
    }

    let universe = mpi::initialize().expect("MPI_Init failed");
    let world = universe.world();
    let rank = world.rank();
    let size = world.size();

    let shared_size = unsafe {
        let mut shared_comm: ffi::MPI_Comm = std::mem::zeroed();
        let rc = ffi::MPI_Comm_split_type(
            world.as_raw(),
            ffi::MPI_COMM_TYPE_SHARED as i32,
            rank,
            ffi::RSMPI_INFO_NULL,
            &mut shared_comm,
        );
        assert_eq!(rc, ffi::MPI_SUCCESS as i32, "MPI_Comm_split_type failed");
        let mut shared_size: i32 = 0;
        ffi::MPI_Comm_size(shared_comm, &mut shared_size);
        ffi::MPI_Comm_free(&mut shared_comm);
        shared_size
    };

    if shared_size > 1 {
        eprintln!(
            "rank {rank}: detected {shared_size} ranks on this node; \
             azcp-cluster requires more than one rank per node \
             (use mpirun -N 1 / one process per host)"
        );
        unsafe { ffi::MPI_Abort(world.as_raw(), 2) };
        return ExitCode::from(2);
    }

    eprintln!("hello rank {rank}/{size} from {}", hostname());

    if rank == 0 {
        print_transport_diagnostics();
    }

    if matches!(args.stage, Stage::Init) {
        return ExitCode::SUCCESS;
    }

    let t_total_start = environment::time();

    let t0 = environment::time();
    let entries = match stages::list::run(&world, &args) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("rank {rank}: list stage failed: {err:#}");
            unsafe { ffi::MPI_Abort(world.as_raw(), 3) };
            return ExitCode::from(3);
        }
    };
    let t_list = environment::time() - t0;
    let total_bytes: u64 = entries
        .iter()
        .map(|e| {
            e.properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0)
        })
        .sum();
    if rank == 0 {
        println!(
            "[list] {} files, {} bytes T={:.2}s",
            entries.len(),
            total_bytes,
            t_list
        );
    }
    if matches!(args.stage, Stage::List) {
        return ExitCode::SUCCESS;
    }

    let t0 = environment::time();
    let presence = match stages::presence::run(&world, &args, &entries) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("rank {rank}: presence stage failed: {err:#}");
            unsafe { ffi::MPI_Abort(world.as_raw(), 4) };
            return ExitCode::from(4);
        }
    };
    let classification = stages::diff::classify(&presence);
    let t_diff = environment::time() - t0;
    if rank == 0 {
        println!(
            "[diff] {} to transfer ({} peer, {} azure), {} skipped T={:.2}s",
            classification.bcast_only.len() + classification.needs_download.len(),
            classification.bcast_only.len(),
            classification.needs_download.len(),
            classification.skipped,
            t_diff
        );
    }

    let download_entries: Vec<BlobItem> = classification
        .needs_download
        .iter()
        .map(|&i| entries[i].clone())
        .collect();

    let total_ranks = size as usize;
    let download_rank_count = match args.download_ranks {
        Some(k) if k == 0 || k > total_ranks => {
            if rank == 0 {
                eprintln!("[fatal] --download-ranks {k} invalid (must be 1..={total_ranks})");
            }
            unsafe { ffi::MPI_Abort(world.as_raw(), 7) };
            return ExitCode::from(7);
        }
        Some(k) => k,
        None => total_ranks,
    };
    let per_rank_bandwidth = args
        .max_bandwidth
        .map(|bw| (bw / download_rank_count as u64).max(1));
    let is_downloader = (rank as usize) < download_rank_count;
    if rank == 0 && download_rank_count < total_ranks {
        println!(
            "[shard] {download_rank_count}/{total_ranks} ranks will download; \
             remaining {} ranks bcast-only",
            total_ranks - download_rank_count
        );
    }
    let download_owners = stages::owners::compute(&download_entries, download_rank_count);

    let mut my_entries: Vec<BlobItem> = Vec::new();
    let mut bcast_plan: Vec<(usize, usize)> =
        Vec::with_capacity(classification.bcast_only.len() + classification.needs_download.len());
    for (sub_idx, &file_idx) in classification.needs_download.iter().enumerate() {
        let owner = download_owners[sub_idx];
        if owner == rank as usize {
            my_entries.push(entries[file_idx].clone());
        }
        bcast_plan.push((file_idx, owner));
    }
    bcast_plan.extend(classification.bcast_only.iter().copied());
    bcast_plan.sort_by_key(|(idx, _)| *idx);

    // Resolve effective shard_size: explicit --shard-size wins; else
    // derive from --file-shards over the largest downloaded file; else 0
    // (one shard per file, v0.3.1 plan shape).
    let bcast_chunk_u64 = args.bcast_chunk as u64;
    let effective_shard_size: u64 = if args.shard_size > 0 {
        args.shard_size
    } else if let Some(n) = args.file_shards {
        stages::shards::compute_shard_size_from_file_shards(
            &download_entries,
            n,
            bcast_chunk_u64,
        )
    } else {
        0
    };
    if rank == 0 && effective_shard_size > 0 {
        println!(
            "[shard] range-sharding active: shard_size={} bytes ({} chunks/shard)",
            effective_shard_size,
            effective_shard_size / bcast_chunk_u64
        );
    }

    // Build the range-shard plan over downloaded entries; LPT assigns
    // each shard to one of the download ranks. Then re-key file_idx
    // back to the original `entries[]` index space and append the
    // bcast_only entries (presence-determined broadcaster, 1 shard each).
    let download_shards =
        stages::shards::compute_shards(&download_entries, download_rank_count, effective_shard_size);

    let mut bcast_shards: Vec<stages::shards::ShardSpec> = Vec::with_capacity(
        download_shards.len() + classification.bcast_only.len(),
    );
    for s in &download_shards {
        let entries_idx = classification.needs_download[s.file_idx];
        bcast_shards.push(stages::shards::ShardSpec {
            file_idx: entries_idx,
            shard_idx: s.shard_idx,
            byte_offset: s.byte_offset,
            byte_len: s.byte_len,
            owner_rank: s.owner_rank,
        });
    }
    for &(file_idx, owner_rank) in &classification.bcast_only {
        let byte_len = entries[file_idx]
            .properties
            .as_ref()
            .and_then(|p| p.content_length)
            .unwrap_or(0);
        bcast_shards.push(stages::shards::ShardSpec {
            file_idx,
            shard_idx: 0,
            byte_offset: 0,
            byte_len,
            owner_rank,
        });
    }
    bcast_shards.sort_by_key(|s| (s.file_idx, s.shard_idx));

    // Per-rank download ranges: this rank downloads the byte-ranges of
    // the shards it owns. Local paths absolute under args.dest.
    let my_ranges: Vec<DownloadRange> = download_shards
        .iter()
        .filter(|s| s.owner_rank == rank as usize)
        .map(|s| {
            let entry = &download_entries[s.file_idx];
            let local_path = args.dest.join(paths::local_rel(&args.source, &entry.name));
            DownloadRange {
                blob_name: entry.name.clone(),
                local_path,
                byte_offset: s.byte_offset,
                byte_len: s.byte_len,
            }
        })
        .collect();

    // Pre-truncate every file this rank will touch (download or receive)
    // up-front. Both download_ranges and bcast writers expect the file
    // to exist at full size with no in-flight truncate races.
    {
        use std::collections::HashSet;
        let mut files_to_create: HashSet<usize> = HashSet::new();
        for s in &bcast_shards {
            if !presence.has(rank as usize, s.file_idx) {
                files_to_create.insert(s.file_idx);
            }
        }
        for file_idx in files_to_create {
            let entry = &entries[file_idx];
            let size = entry
                .properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0);
            let local_path = args.dest.join(paths::local_rel(&args.source, &entry.name));
            if let Some(parent) = local_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "rank {rank}: mkdir -p {} failed: {e}",
                        parent.display()
                    );
                    unsafe { ffi::MPI_Abort(world.as_raw(), 8) };
                    return ExitCode::from(8);
                }
            }
            let f = match std::fs::File::create(&local_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "rank {rank}: create {} failed: {e}",
                        local_path.display()
                    );
                    unsafe { ffi::MPI_Abort(world.as_raw(), 8) };
                    return ExitCode::from(8);
                }
            };
            if let Err(e) = f.set_len(size) {
                eprintln!(
                    "rank {rank}: ftruncate {} to {} failed: {e}",
                    local_path.display(),
                    size
                );
                unsafe { ffi::MPI_Abort(world.as_raw(), 8) };
                return ExitCode::from(8);
            }
        }
    }

    let my_bytes: u64 = my_ranges.iter().map(|r| r.byte_len).sum();
    let download_bytes: u64 = download_entries
        .iter()
        .map(|e| {
            e.properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0)
        })
        .sum();
    let bcast_bytes: u64 = bcast_shards.iter().map(|s| s.byte_len).sum();
    eprintln!(
        "[shard] rank {rank}: {} ranges, {} bytes",
        my_ranges.len(),
        my_bytes
    );

    let t_download = if matches!(args.stage, Stage::Bcast) || !is_downloader {
        0.0
    } else {
        let t0 = environment::time();
        if let Err(err) = stages::download::run(&args, my_ranges.clone(), per_rank_bandwidth) {
            eprintln!("rank {rank}: download stage failed: {err:#}");
            unsafe { ffi::MPI_Abort(world.as_raw(), 5) };
            return ExitCode::from(5);
        }
        environment::time() - t0
    };
    if rank == 0 {
        println!(
            "[download] {} bytes T={:.2}s BW={}",
            download_bytes,
            t_download,
            timing::human_bw(download_bytes, t_download)
        );
    }
    if matches!(args.stage, Stage::Download) {
        return ExitCode::SUCCESS;
    }

    // Synchronize all ranks before bcast: each receiver opens its
    // pre-truncated file with O_DIRECT, and downloaders must have
    // finished pwriting before owners read from those files.
    world.barrier();

    let t0 = environment::time();
    if let Err(err) = stages::broadcast::run(&world, &args, &entries, &bcast_shards, &presence) {
        eprintln!("rank {rank}: broadcast stage failed: {err:#}");
        unsafe { ffi::MPI_Abort(world.as_raw(), 6) };
        return ExitCode::from(6);
    }
    let t_bcast = environment::time() - t0;
    if rank == 0 {
        println!(
            "[bcast] {} shards {} bytes T={:.2}s BW={}",
            bcast_shards.len(),
            bcast_bytes,
            t_bcast,
            timing::human_bw(bcast_bytes, t_bcast)
        );
    }

    if args.verify {
        if let Err(err) = stages::verify::run(&world, &args, &entries) {
            eprintln!("rank {rank}: verify stage failed: {err:#}");
            unsafe { ffi::MPI_Abort(world.as_raw(), 7) };
            return ExitCode::from(7);
        }
    }

    let t0 = environment::time();
    if rank == 0 {
        if let Some(path) = &args.save_filelist {
            let text = filelist::serialize(&entries);
            if let Err(e) = std::fs::write(path, text) {
                eprintln!("warning: --save-filelist {} failed: {e}", path.display());
            }
        }
    }
    let t_filelist = environment::time() - t0;
    if rank == 0 {
        let saved = args.save_filelist.is_some();
        println!(
            "[filelist] {} T={:.2}s",
            if saved {
                format!("wrote {} entries", entries.len())
            } else {
                "skipped (no --save-filelist)".to_string()
            },
            t_filelist
        );
    }

    let t_total = environment::time() - t_total_start;
    if rank == 0 {
        println!("[total] T={:.2}s", t_total);
    }

    ExitCode::SUCCESS
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn print_transport_diagnostics() {
    eprintln!("[transport] env:");
    for var in [
        "OMPI_MCA_pml",
        "OMPI_MCA_btl",
        "OMPI_MCA_osc",
        "UCX_TLS",
        "UCX_NET_DEVICES",
        "UCX_IB_GID_INDEX",
    ] {
        let val = std::env::var(var).unwrap_or_else(|_| "(unset)".to_string());
        eprintln!("[transport]   {var}={val}");
    }

    if let Ok(output) = std::process::Command::new("ompi_info")
        .args(["--param", "pml", "all", "--level", "9"])
        .output()
    {
        let s = String::from_utf8_lossy(&output.stdout);
        let active: Vec<&str> = s
            .lines()
            .filter(|l| l.contains("MCA pml:") && l.contains("---"))
            .collect();
        if !active.is_empty() {
            eprintln!("[transport] ompi_info pml components: {}", active.len());
        }
    }

    if let Ok(output) = std::process::Command::new("ucx_info").arg("-d").output() {
        let s = String::from_utf8_lossy(&output.stdout);
        let devices: Vec<&str> = s
            .lines()
            .filter(|l| l.starts_with("# Memory domain:") || l.contains("Transport:"))
            .collect();
        eprintln!("[transport] ucx_info -d ({} entries):", devices.len());
        for d in devices.iter().take(40) {
            eprintln!("[transport]   {}", d.trim());
        }
    } else {
        eprintln!("[transport] ucx_info not available");
    }
}
