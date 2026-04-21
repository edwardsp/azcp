use std::path::PathBuf;

pub const DEFAULT_BLOCK_SIZE: u64 = 4 * 1024 * 1024;
pub const MAX_BLOCK_COUNT: u64 = 50_000;
pub const DEFAULT_CONCURRENCY: usize = 64;
pub const DEFAULT_MAX_RETRIES: u32 = 5;
pub const API_VERSION: &str = "2024-11-04";

pub struct TransferConfig {
    pub block_size: u64,
    pub concurrency: usize,
    pub dry_run: bool,
    pub overwrite: bool,
    pub recursive: bool,
    pub include_pattern: Option<String>,
    pub exclude_pattern: Option<String>,
    pub check_md5: bool,
    pub progress: bool,
    pub max_retries: u32,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            concurrency: DEFAULT_CONCURRENCY,
            dry_run: false,
            overwrite: true,
            recursive: false,
            include_pattern: None,
            exclude_pattern: None,
            check_md5: false,
            progress: false,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

pub fn log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("azcp")
        .join("logs")
}

pub fn plan_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("azcp")
        .join("plans")
}
