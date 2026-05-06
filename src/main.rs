use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use azcp::cli;
use azcp::cli::args::{Cli, Command, CopyArgs};
use azcp::cli::copy::SharedTransfer;
use azcp::engine::progress::TransferProgress;
use azcp::engine::rate_limiter::RateLimiter;
use azcp::engine::TransferSummary;
use azcp::error::Result;
use azcp::storage::blob::client::{LatencyStats, RetryStats};

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

    let shared_progress = Arc::new(TransferProgress::new(
        0,
        0,
        azcp::cli::args::resolve_progress(args.progress, args.no_progress),
    ));
    let shared_retry = Arc::new(RetryStats::default());
    let shared_latency = Arc::new(LatencyStats::default());
    let shared_rate = args.max_bandwidth.map(RateLimiter::new);

    let started = std::time::Instant::now();

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let mut a = args.clone();
            a.shard = Some((i, n));
            a.workers = 1;
            a.max_bandwidth = None;
            let shared = SharedTransfer {
                progress: Some(shared_progress.clone()),
                retry_stats: Some(shared_retry.clone()),
                latency_stats: Some(shared_latency.clone()),
                rate_limiter: shared_rate.clone(),
            };
            std::thread::Builder::new()
                .name(format!("azcp-w{i}"))
                .spawn(move || -> Result<Option<TransferSummary>> {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_name(format!("azcp-w{i}-rt"))
                        .build()
                        .expect("build worker runtime");
                    rt.block_on(cli::copy::run_with_shared(&a, shared))
                })
                .expect("spawn worker thread")
        })
        .collect();

    let mut first_err: Option<azcp::error::AzcpError> = None;
    let mut combined = TransferSummary {
        total_files: 0,
        total_bytes: 0,
        succeeded: 0,
        failed: 0,
        skipped: 0,
    };
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok(Some(s))) => {
                combined.total_files += s.total_files;
                combined.total_bytes += s.total_bytes;
                combined.succeeded += s.succeeded;
                combined.failed += s.failed;
                combined.skipped += s.skipped;
            }
            Ok(Ok(None)) => {}
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

    shared_progress.finish();

    cli::copy::print_summary("Transfer", &combined, started.elapsed());
    cli::copy::print_stats(&shared_retry, &shared_latency);

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
