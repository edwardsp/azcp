use clap::Parser;
use tracing_subscriber::EnvFilter;

use azcp::cli;
use azcp::cli::args::{Cli, Command, CopyArgs};
use azcp::error::Result;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "mimalloc-allocator", not(feature = "jemalloc")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let cli = Cli::parse();

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    #[cfg(feature = "profile")]
    let profiler = std::env::var("AZCP_PROFILE_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .and_then(|_| {
            pprof::ProfilerGuardBuilder::default()
                .frequency(997)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
                .ok()
        });
    #[cfg(feature = "profile")]
    if profiler.is_some() {
        eprintln!("[profile] sampling at 997 Hz");
    }

    // Multi-worker path: one process, N independent tokio runtimes, each with its
    // own reqwest connection pool, sharded workload. Avoids the single-runtime
    // scheduler/pool contention that caps single-proc downloads at ~28 Gbps.
    let result = match &cli.command {
        Command::Copy(args) if args.workers > 1 => run_workers(args),
        _ => {
            let rt = tokio::runtime::Runtime::new().expect("build tokio runtime");
            rt.block_on(cli::dispatch(&cli))
        }
    };

    #[cfg(feature = "profile")]
    if let Some(g) = profiler {
        let dump_path =
            std::env::var("AZCP_PROFILE_OUT").unwrap_or_else(|_| "flamegraph.svg".into());
        match g.report().build() {
            Ok(report) => match std::fs::File::create(&dump_path) {
                Ok(f) => match report.flamegraph(f) {
                    Ok(_) => eprintln!("[profile] wrote {dump_path}"),
                    Err(e) => eprintln!("[profile] flamegraph write failed: {e}"),
                },
                Err(e) => eprintln!("[profile] cannot create {dump_path}: {e}"),
            },
            Err(e) => eprintln!("[profile] report build failed: {e}"),
        }
    }

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run_workers(args: &CopyArgs) -> Result<()> {
    let n = args.workers;
    if args.shard.is_some() {
        eprintln!("Warning: --shard ignored when --workers > 1 (workers auto-shard).");
    }
    eprintln!("[workers] spawning {n} independent runtimes");

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let mut a = args.clone();
            a.shard = Some((i, n));
            a.workers = 1;
            if n > 1 {
                a.progress = false;
            }
            std::thread::Builder::new()
                .name(format!("azcp-w{i}"))
                .spawn(move || -> Result<()> {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_name(format!("azcp-w{i}-rt"))
                        .build()
                        .expect("build worker runtime");
                    rt.block_on(cli::copy::run(&a))
                })
                .expect("spawn worker thread")
        })
        .collect();

    let mut first_err: Option<azcp::error::AzcpError> = None;
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("[workers] worker {i} failed: {e}");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(_) => {
                eprintln!("[workers] worker {i} panicked");
                if first_err.is_none() {
                    first_err = Some(azcp::error::AzcpError::Transfer(format!(
                        "worker {i} panicked"
                    )));
                }
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
