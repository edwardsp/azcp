pub mod az_cli;
pub mod sas;
pub mod shared_key;

use tracing::debug;

use crate::error::Result;

#[derive(Clone)]
pub enum Credential {
    SharedKey { account: String, key: String },
    Sas { token: String },
    Bearer { token: String },
    Anonymous,
}

impl Credential {
    pub fn from_env() -> Result<Option<Self>> {
        if let (Ok(account), Ok(key)) = (
            std::env::var("AZURE_STORAGE_ACCOUNT"),
            std::env::var("AZURE_STORAGE_KEY"),
        ) {
            return Ok(Some(Credential::SharedKey { account, key }));
        }
        if let Ok(sas) = std::env::var("AZURE_STORAGE_SAS_TOKEN") {
            return Ok(Some(Credential::Sas { token: sas }));
        }
        Ok(None)
    }

    pub fn from_env_or_cli(account_name: &str) -> Result<Option<Self>> {
        if let Some(cred) = Self::from_env()? {
            return Ok(Some(cred));
        }

        debug!("no env credentials, trying az CLI account key for {account_name}");
        match az_cli::get_account_key(account_name) {
            Ok(key) => {
                return Ok(Some(Credential::SharedKey {
                    account: account_name.to_string(),
                    key,
                }));
            }
            Err(e) => {
                debug!("az account key failed: {e}, trying Bearer token");
            }
        }

        match az_cli::get_storage_token() {
            Ok(token) => Ok(Some(Credential::Bearer { token })),
            Err(e) => {
                debug!("az Bearer token failed: {e}");
                Ok(None)
            }
        }
    }
}
