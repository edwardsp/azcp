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

    if let Err(e) = cli::dispatch(&cli).await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
