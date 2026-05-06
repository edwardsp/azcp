pub mod az_cli;
pub mod imds;
pub mod sas;
pub mod shared_key;

use std::sync::{Mutex, OnceLock};

use tracing::warn;

use crate::error::Result;

#[derive(Clone)]
pub enum Credential {
    SharedKey { account: String, key: String },
    Sas { token: String },
    Bearer { token: String },
    Anonymous,
}

fn ambient_cache() -> &'static Mutex<Option<Credential>> {
    static C: OnceLock<Mutex<Option<Credential>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
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

        // Single-flight cache: workers spawned by `--workers N` all call this
        // concurrently. Without serialization, each races IMDS, which rate-limits
        // and drops some requests → silent fallthrough to Anonymous → 401. Holding
        // the lock across the resolve makes exactly one IMDS round-trip per process;
        // the Bearer is valid 24h so reuse is correct.
        let mut guard = ambient_cache().lock().expect("ambient cred mutex poisoned");
        if let Some(c) = guard.as_ref() {
            return Ok(Some(c.clone()));
        }
        let resolved = Self::resolve_ambient(account_name)?;
        if let Some(ref c) = resolved {
            *guard = Some(c.clone());
        }
        Ok(resolved)
    }

    pub fn resolve_or_anonymous(account_name: &str, sas_token: Option<&str>) -> Result<Self> {
        if let Some(sas) = sas_token {
            return Ok(Credential::Sas {
                token: sas.to_string(),
            });
        }
        if let Some(cred) = Self::from_env_or_cli(account_name)? {
            return Ok(cred);
        }
        warn!(
            "no credentials found for account {account_name}; using anonymous access \
             (will fail with 401 on private containers). Set AZURE_STORAGE_SAS_TOKEN, \
             run `az login`, or attach a managed identity."
        );
        Ok(Credential::Anonymous)
    }

    fn resolve_ambient(account_name: &str) -> Result<Option<Self>> {
        // Ok(None) = auth method not applicable (env vars absent, IMDS unreachable)
        // → fall through. Err = method attempted and failed (rate-limit, 5xx, parse)
        // → propagate so caller sees a real auth failure instead of silently
        // degrading to Anonymous and getting 401 NoAuthenticationInformation.
        match imds::get_storage_token_workload() {
            Ok(Some(token)) => return Ok(Some(Credential::Bearer { token })),
            Ok(None) => {}
            Err(e) => return Err(e),
        }

        match imds::get_storage_token_imds() {
            Ok(Some(token)) => return Ok(Some(Credential::Bearer { token })),
            Ok(None) => {}
            Err(e) => return Err(e),
        }

        // Prefer az-cli Bearer (RBAC) over az-cli scraped SharedKey: many
        // accounts disable `allowSharedKeyAccess`, in which case `az storage
        // account keys list` still returns a key (management plane) but Azure
        // rejects it on data-plane use with 403 KeyBasedAuthenticationNotPermitted.
        if let Ok(token) = az_cli::get_storage_token() {
            return Ok(Some(Credential::Bearer { token }));
        }

        if let Ok(key) = az_cli::get_account_key(account_name) {
            return Ok(Some(Credential::SharedKey {
                account: account_name.to_string(),
                key,
            }));
        }

        Ok(None)
    }
}
