use thiserror::Error;

#[derive(Error, Debug)]
pub enum AzcpError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("Azure Storage ({status}): {message}")]
    Storage { status: u16, message: String },

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Transfer failed: {0}")]
    Transfer(String),

    #[error("Destination blob already exists: {0}")]
    AlreadyExists(String),

    #[error("MD5 mismatch for {path}: expected {expected}, got {actual}")]
    Md5Mismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("XML parse: {0}")]
    Xml(String),
}

pub type Result<T> = std::result::Result<T, AzcpError>;
