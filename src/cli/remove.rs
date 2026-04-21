use std::sync::Arc;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::auth::Credential;
use crate::engine::build_glob_set;
use crate::error::Result;
use crate::storage::blob::client::BlobClient;
use crate::storage::location::{self, BlobLocation, Location};

use super::args::RemoveArgs;

pub async fn run(args: &RemoveArgs) -> Result<()> {
    let location = location::parse_location(&args.url)?;

    match location {
        Location::AzureBlob(loc) => remove_azure(&loc, args).await,
        Location::Local(_) => {
            eprintln!("Use system tools to remove local files.");
            Ok(())
        }
    }
}

async fn remove_azure(loc: &BlobLocation, args: &RemoveArgs) -> Result<()> {
    let credential = resolve_credential(loc)?;
    let client = Arc::new(BlobClient::with_max_retries(credential, args.max_retries)?);

    if loc.path.is_empty() && loc.container.is_empty() {
        eprintln!("Specify a container or blob to delete.");
        return Ok(());
    }

    if loc.path.is_empty() {
        if args.dry_run {
            println!("[dry-run] would delete container: {}", loc.container);
        } else {
            client
                .delete_container(&loc.account, &loc.container)
                .await?;
            println!("Deleted container: {}", loc.container);
        }
        return Ok(());
    }

    if !args.recursive {
        if args.dry_run {
            println!("[dry-run] would delete: {}", loc.path);
        } else {
            client
                .delete_blob(&loc.account, &loc.container, &loc.path)
                .await?;
            println!("Deleted: {}/{}", loc.container, loc.path);
        }
        return Ok(());
    }

    let include = build_glob_set(args.include_pattern.as_deref())?;
    let exclude = build_glob_set(args.exclude_pattern.as_deref())?;

    let mut blobs = client
        .list_blobs(&loc.account, &loc.container, Some(&loc.path), true)
        .await?;
    blobs.retain(|b| {
        let relative = b
            .name
            .strip_prefix(&loc.path)
            .unwrap_or(&b.name)
            .trim_start_matches('/');
        if let Some(inc) = &include {
            if !inc.is_match(relative) {
                return false;
            }
        }
        if let Some(exc) = &exclude {
            if exc.is_match(relative) {
                return false;
            }
        }
        true
    });

    let total = blobs.len() as u64;

    if args.dry_run {
        for blob in &blobs {
            println!("[dry-run] would delete: {}", blob.name);
        }
        println!("\n{total} blob(s) would be deleted");
        return Ok(());
    }

    let pb = if args.progress {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blobs ({per_sec}) ETA {eta:>5}",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.enable_steady_tick(std::time::Duration::from_millis(150));
        pb
    } else {
        let pb = ProgressBar::hidden();
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb
    };

    let sem = Arc::new(Semaphore::new(args.concurrency));
    let mut tasks: JoinSet<Result<String>> = JoinSet::new();

    for blob in blobs {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let acct = loc.account.clone();
        let ctr = loc.container.clone();
        let name = blob.name.clone();
        let pb_c = pb.clone();
        tasks.spawn(async move {
            let _permit = permit;
            client.delete_blob(&acct, &ctr, &name).await?;
            pb_c.inc(1);
            Ok(name)
        });
    }

    let mut deleted = 0u64;
    let mut failed = 0u64;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(_)) => deleted += 1,
            Ok(Err(e)) => {
                failed += 1;
                if args.progress {
                    pb.println(format!("ERROR: {e}"));
                } else {
                    eprintln!("ERROR: {e}");
                }
            }
            Err(e) => {
                failed += 1;
                if args.progress {
                    pb.println(format!("ERROR: {e}"));
                } else {
                    eprintln!("ERROR: {e}");
                }
            }
        }
    }

    pb.finish_and_clear();
    println!("\n{deleted} blob(s) deleted, {failed} failed");

    if failed > 0 {
        return Err(crate::error::AzcpError::Transfer(format!(
            "{failed} blob deletion(s) failed"
        )));
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
