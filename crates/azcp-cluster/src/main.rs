mod cli;
mod filelist;
mod paths;
mod rails;
mod stages;
mod timing;

use std::process::ExitCode;

use clap::Parser;
use mpi::environment;
use mpi::ffi;
use mpi::traits::*;

use azcp::BlobItem;

use cli::{Args, Stage};

fn main() -> ExitCode {
    let args = Args::parse();

    let rail_config = match resolve_rails(args.bcast_rails.as_deref()) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("[fatal] --bcast-rails: {err}");
            return ExitCode::from(2);
        }
    };
    apply_rail_env(&rail_config);

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
        print_transport_diagnostics(&rail_config);
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

    let my_bytes: u64 = my_entries
        .iter()
        .map(|e| {
            e.properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0)
        })
        .sum();
    let download_bytes: u64 = download_entries
        .iter()
        .map(|e| {
            e.properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0)
        })
        .sum();
    let bcast_bytes: u64 = bcast_plan
        .iter()
        .map(|(idx, _)| {
            entries[*idx]
                .properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0)
        })
        .sum();
    eprintln!(
        "[shard] rank {rank}: {} files, {} bytes",
        my_entries.len(),
        my_bytes
    );

    let t_download = if matches!(args.stage, Stage::Bcast) || !is_downloader {
        0.0
    } else {
        let t0 = environment::time();
        if let Err(err) = stages::download::run(&args, my_entries.clone(), per_rank_bandwidth) {
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

    let t0 = environment::time();
    if let Err(err) = stages::broadcast::run(
        &world,
        &args,
        &entries,
        &bcast_plan,
        &presence,
        rail_config.count,
    ) {
        eprintln!("rank {rank}: broadcast stage failed: {err:#}");
        unsafe { ffi::MPI_Abort(world.as_raw(), 6) };
        return ExitCode::from(6);
    }
    let t_bcast = environment::time() - t0;
    if rank == 0 {
        println!(
            "[bcast] {} files {} bytes T={:.2}s BW={}",
            bcast_plan.len(),
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

fn print_transport_diagnostics(rails: &RailConfig) {
    eprintln!("[transport] env:");
    for var in [
        "OMPI_MCA_pml",
        "OMPI_MCA_btl",
        "OMPI_MCA_osc",
        "UCX_TLS",
        "UCX_NET_DEVICES",
        "UCX_IB_GID_INDEX",
        "UCX_MAX_RNDV_RAILS",
        "UCX_MAX_EAGER_RAILS",
    ] {
        let val = std::env::var(var).unwrap_or_else(|_| "(unset)".to_string());
        eprintln!("[transport]   {var}={val}");
    }

    if rails.count > 1 {
        eprintln!(
            "[transport] multi-rail: K={} devices=[{}]",
            rails.count,
            rails.devices.join(",")
        );
    } else {
        eprintln!("[transport] multi-rail: disabled (single rail)");
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

struct RailConfig {
    count: usize,
    devices: Vec<String>,
}

fn resolve_rails(spec: Option<&str>) -> Result<RailConfig, String> {
    let Some(spec) = spec else {
        return Ok(RailConfig {
            count: 1,
            devices: Vec::new(),
        });
    };
    let detected = rails::enumerate_active_ib_hcas()
        .map_err(|e| format!("scan /sys/class/infiniband: {e}"))?;
    if detected.is_empty() {
        return Err(format!(
            "--bcast-rails {spec}: no ACTIVE InfiniBand ports found in /sys/class/infiniband"
        ));
    }

    let want: usize = if spec.eq_ignore_ascii_case("auto") {
        detected.len()
    } else {
        spec.parse::<usize>()
            .map_err(|_| format!("--bcast-rails: expected integer or 'auto', got {spec:?}"))?
    };
    if want == 0 {
        return Err("--bcast-rails 0 is invalid (use 'auto' or N>=1)".into());
    }
    if want > detected.len() {
        return Err(format!(
            "--bcast-rails {want}: only {} ACTIVE IB ports detected ({})",
            detected.len(),
            detected.join(",")
        ));
    }
    let devices: Vec<String> = detected.into_iter().take(want).collect();
    Ok(RailConfig {
        count: want,
        devices,
    })
}

fn apply_rail_env(cfg: &RailConfig) {
    if cfg.count <= 1 {
        return;
    }
    let n = cfg.count.to_string();
    set_env_if_unset("UCX_MAX_RNDV_RAILS", &n);
    set_env_if_unset("UCX_MAX_EAGER_RAILS", &n);
    let joined = cfg.devices.join(",");
    set_env_if_unset("UCX_NET_DEVICES", &joined);
}

fn set_env_if_unset(key: &str, val: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, val);
    }
}
