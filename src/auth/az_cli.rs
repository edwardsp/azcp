use serde::Deserialize;
use std::process::Command;

use crate::error::AzcpError;

#[derive(Deserialize)]
struct AzTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[derive(Deserialize)]
struct AzKeyEntry {
    value: String,
}

pub fn get_storage_token() -> Result<String, AzcpError> {
    let output = Command::new("az")
        .args([
            "account",
            "get-access-token",
            "--resource",
            "https://storage.azure.com/",
            "--query",
            "{accessToken: accessToken}",
            "-o",
            "json",
        ])
        .output()
        .map_err(|e| AzcpError::Auth(format!("failed to run `az` CLI: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AzcpError::Auth(format!("az CLI token error: {stderr}")));
    }

    let resp: AzTokenResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| AzcpError::Auth(format!("failed to parse az token: {e}")))?;

    Ok(resp.access_token)
}

pub fn get_account_key(account_name: &str) -> Result<String, AzcpError> {
    let output = Command::new("az")
        .args([
            "storage",
            "account",
            "keys",
            "list",
            "--account-name",
            account_name,
            "--query",
            "[0]",
            "-o",
            "json",
        ])
        .output()
        .map_err(|e| AzcpError::Auth(format!("failed to run `az` CLI: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AzcpError::Auth(format!(
            "az storage account keys list failed: {stderr}"
        )));
    }

    let entry: AzKeyEntry = serde_json::from_slice(&output.stdout)
        .map_err(|e| AzcpError::Auth(format!("failed to parse az key response: {e}")))?;

    Ok(entry.value)
}
