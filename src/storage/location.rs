use url::Url;

use crate::auth::sas;
use crate::error::{AzcpError, Result};

#[derive(Debug, Clone)]
pub enum Location {
    Local(String),
    AzureBlob(BlobLocation),
}

#[derive(Debug, Clone)]
pub struct BlobLocation {
    pub account: String,
    pub container: String,
    pub path: String,
    pub sas_token: Option<String>,
}

impl BlobLocation {
    pub fn service_url(&self) -> String {
        format!("https://{}.blob.core.windows.net", self.account)
    }

    pub fn container_url(&self) -> String {
        format!("{}/{}", self.service_url(), self.container)
    }
}

pub fn parse_location(input: &str) -> Result<Location> {
    if input.starts_with("https://") || input.starts_with("http://") {
        parse_azure_url(input).map(Location::AzureBlob)
    } else {
        Ok(Location::Local(input.to_string()))
    }
}

fn parse_azure_url(input: &str) -> Result<BlobLocation> {
    let url = Url::parse(input).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

    let host = url
        .host_str()
        .ok_or_else(|| AzcpError::InvalidUrl("missing host".into()))?;

    let account = host
        .strip_suffix(".blob.core.windows.net")
        .ok_or_else(|| {
            AzcpError::InvalidUrl(format!("expected *.blob.core.windows.net, got {host}"))
        })?
        .to_string();

    let path = url.path().trim_start_matches('/');
    let mut parts = path.splitn(2, '/');
    let container = parts.next().unwrap_or("").to_string();
    let blob_path = parts.next().unwrap_or("").to_string();

    let sas_token = sas::extract_sas_token(&url);

    Ok(BlobLocation {
        account,
        container,
        path: blob_path,
        sas_token,
    })
}
