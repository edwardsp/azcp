use clap::Parser;
use tracing_subscriber::EnvFilter;

use azcp::cli;
use azcp::cli::args::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cli.log_level));

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

    let result = cli::dispatch(&cli).await;

    #[cfg(feature = "profile")]
    if let Some(g) = profiler {
        let dump_path = std::env::var("AZCP_PROFILE_OUT").unwrap_or_else(|_| "flamegraph.svg".into());
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