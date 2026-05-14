pub mod auth;
pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod storage;

/// `User-Agent` header value sent on every outbound HTTP request
/// (Azure Blob REST, IMDS, federated-token exchange). Kept as a
/// single source of truth so server-side attribution / capacity
/// triage can identify azcp traffic by version.
pub const USER_AGENT: &str = concat!("azcp/", env!("CARGO_PKG_VERSION"));

pub use auth::Credential;
pub use config::TransferConfig;
pub use engine::{apply_shard, read_shardlist, DownloadRange, TransferEngine};
pub use error::{AzcpError, Result};
pub use storage::blob::models::{BlobItem, BlobProperties};
pub use storage::blob::BlobClient;
pub use storage::location::{parse_location, BlobLocation, Location};

#[cfg(test)]
mod user_agent_tests {
    #[test]
    fn user_agent_format_is_azcp_slash_version() {
        assert!(super::USER_AGENT.starts_with("azcp/"));
        let version = &super::USER_AGENT["azcp/".len()..];
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert!(!version.is_empty());
    }
}
