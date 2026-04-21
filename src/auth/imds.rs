use serde::Deserialize;
use std::time::Duration;

use crate::error::AzcpError;

// Azure Instance Metadata Service endpoint. Reachable only from within an
// Azure VM; the Metadata:true header is required and the IP is fixed.
const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const IMDS_API_VERSION: &str = "2018-02-01";
const STORAGE_RESOURCE: &str = "https://storage.azure.com/";

// AKS workload-identity injects these env vars into pods bound to a
// federated credential. When present we exchange the projected SA token
// for a user-delegation Entra token via the standard OAuth2 flow.
const WORKLOAD_CLIENT_ID: &str = "AZURE_CLIENT_ID";
const WORKLOAD_TENANT_ID: &str = "AZURE_TENANT_ID";
const WORKLOAD_TOKEN_FILE: &str = "AZURE_FEDERATED_TOKEN_FILE";
const WORKLOAD_AUTHORITY: &str = "AZURE_AUTHORITY_HOST";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

pub fn get_storage_token_workload() -> Result<Option<String>, AzcpError> {
    let (Ok(client_id), Ok(tenant_id), Ok(token_file)) = (
        std::env::var(WORKLOAD_CLIENT_ID),
        std::env::var(WORKLOAD_TENANT_ID),
        std::env::var(WORKLOAD_TOKEN_FILE),
    ) else {
        return Ok(None);
    };
    let authority = std::env::var(WORKLOAD_AUTHORITY)
        .unwrap_or_else(|_| "https://login.microsoftonline.com/".into());
    let assertion = std::fs::read_to_string(&token_file)
        .map_err(|e| AzcpError::Auth(format!("read federated token {token_file}: {e}")))?;
    let assertion = assertion.trim();

    let url = format!(
        "{}/{}/oauth2/v2.0/token",
        authority.trim_end_matches('/'),
        tenant_id
    );
    let params = [
        ("client_id", client_id.as_str()),
        ("scope", "https://storage.azure.com/.default"),
        ("grant_type", "client_credentials"),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ),
        ("client_assertion", assertion),
    ];
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AzcpError::Auth(format!("http client: {e}")))?;
    let resp = client
        .post(&url)
        .form(&params)
        .send()
        .map_err(|e| AzcpError::Auth(format!("workload token exchange: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| AzcpError::Auth(format!("read token body: {e}")))?;
    if !status.is_success() {
        return Err(AzcpError::Auth(format!(
            "workload token HTTP {status}: {body}"
        )));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| AzcpError::Auth(format!("parse workload token: {e} body={body}")))?;
    Ok(Some(parsed.access_token))
}

pub fn get_storage_token_imds() -> Result<Option<String>, AzcpError> {
    let client_id = std::env::var(WORKLOAD_CLIENT_ID).ok();
    let mut url =
        format!("{IMDS_ENDPOINT}?api-version={IMDS_API_VERSION}&resource={STORAGE_RESOURCE}");
    if let Some(ref cid) = client_id {
        url.push_str("&client_id=");
        url.push_str(cid);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| AzcpError::Auth(format!("http client: {e}")))?;
    let resp = match client.get(&url).header("Metadata", "true").send() {
        Ok(r) => r,
        // Not running on an Azure VM (or IMDS unreachable) — not an error,
        // just means this auth path doesn't apply. Caller falls through.
        Err(_) => return Ok(None),
    };
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| AzcpError::Auth(format!("read IMDS body: {e}")))?;
    if !status.is_success() {
        return Err(AzcpError::Auth(format!("IMDS HTTP {status}: {body}")));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| AzcpError::Auth(format!("parse IMDS token: {e} body={body}")))?;
    Ok(Some(parsed.access_token))
}
