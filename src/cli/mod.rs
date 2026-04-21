pub mod args;
pub mod copy;
pub mod list;
pub mod make;
pub mod remove;
pub mod sync_cmd;

use crate::error::Result;
use args::{Cli, Command};

pub async fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Copy(args) => copy::run(args).await,
        Command::Sync(args) => sync_cmd::run(args).await,
        Command::List(args) => list::run(args).await,
        Command::Remove(args) => remove::run(args).await,
        Command::Make(args) => make::run(args).await,
        Command::Env => {
            print_env();
            Ok(())
        }
    }
}

fn print_env() {
    println!("azcp environment:\n");
    let vars = [
        ("AZURE_STORAGE_ACCOUNT", "Storage account name"),
        ("AZURE_STORAGE_KEY", "Storage account key (SharedKey auth)"),
        (
            "AZURE_STORAGE_SAS_TOKEN",
            "SAS token for authentication",
        ),
        ("AZCOPY_LOG_LOCATION", "Log file directory"),
        ("AZCOPY_CONCURRENCY_VALUE", "Default concurrency level"),
    ];
    for (name, desc) in &vars {
        let value = std::env::var(name).unwrap_or_else(|_| "(not set)".to_string());
        println!("  {name:<30} = {value}");
        println!("  {:<30}   {desc}", "");
    }
}
