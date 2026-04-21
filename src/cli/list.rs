use crate::auth::Credential;
use crate::error::Result;
use crate::storage::blob::client::BlobClient;
use crate::storage::location::{self, BlobLocation, Location};

use super::args::ListArgs;

pub async fn run(args: &ListArgs) -> Result<()> {
    let location = location::parse_location(&args.url)?;

    match location {
        Location::AzureBlob(loc) => list_azure(&loc, args).await,
        Location::Local(path) => {
            eprintln!("Use system tools to list local path: {path}");
            Ok(())
        }
    }
}

async fn list_azure(loc: &BlobLocation, args: &ListArgs) -> Result<()> {
    let credential = resolve_credential(loc)?;
    let client = BlobClient::new(credential)?;

    if loc.container.is_empty() {
        let containers = client.list_containers(&loc.account).await?;
        for c in &containers {
            let modified = c
                .properties
                .as_ref()
                .and_then(|p| p.last_modified.as_deref())
                .unwrap_or("");
            if args.machine_readable {
                println!("{}\t{modified}", c.name);
            } else {
                println!("  {:<40} {modified}", c.name);
            }
        }
        println!("\n{} container(s)", containers.len());
    } else {
        let prefix = if loc.path.is_empty() {
            None
        } else {
            Some(loc.path.as_str())
        };
        let (blobs, prefixes) = client
            .list_entries(&loc.account, &loc.container, prefix, args.recursive)
            .await?;

        for p in &prefixes {
            if args.machine_readable {
                println!("{p}\t-\tDIR");
            } else {
                println!("  {:<60} {:>12}", p, "<DIR>");
            }
        }

        let mut total_size: u64 = 0;
        for b in &blobs {
            let size = b
                .properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0);
            total_size += size;
            let modified = b
                .properties
                .as_ref()
                .and_then(|p| p.last_modified.as_deref())
                .unwrap_or("");

            if args.machine_readable {
                println!("{}\t{size}\t{modified}", b.name);
            } else {
                println!(
                    "  {:<60} {:>12} {modified}",
                    b.name,
                    humansize::format_size(size, humansize::BINARY),
                );
            }
        }
        println!(
            "\n{} dir(s), {} blob(s), {}",
            prefixes.len(),
            blobs.len(),
            humansize::format_size(total_size, humansize::BINARY)
        );
    }

    Ok(())
}

fn resolve_credential(loc: &BlobLocation) -> Result<Credential> {
    if let Some(ref sas) = loc.sas_token {
        return Ok(Credential::Sas {
            token: sas.clone(),
        });
    }
    if let Some(cred) = Credential::from_env_or_cli(&loc.account)? {
        return Ok(cred);
    }
    Ok(Credential::Anonymous)
}
