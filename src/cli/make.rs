use crate::auth::Credential;
use crate::error::Result;
use crate::storage::blob::client::BlobClient;
use crate::storage::location::{self, BlobLocation, Location};

use super::args::MakeArgs;

pub async fn run(args: &MakeArgs) -> Result<()> {
    let location = location::parse_location(&args.url)?;

    match location {
        Location::AzureBlob(loc) => {
            if loc.container.is_empty() {
                eprintln!("Container name required in URL.");
                return Ok(());
            }
            let credential = resolve_credential(&loc)?;
            let client = BlobClient::new(credential)?;
            client
                .create_container(&loc.account, &loc.container)
                .await?;
            println!("Created container: {}", loc.container);
        }
        Location::Local(path) => {
            tokio::fs::create_dir_all(&path).await?;
            println!("Created directory: {path}");
        }
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
