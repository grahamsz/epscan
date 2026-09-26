// SPDX-License-Identifier: MIT OR Apache-2.0
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("device not found: {0}")]
    NotFound(String),
    #[error("scanner busy: {0}")]
    Busy(String),
    #[error("driver/access configuration: {0}")]
    Driver(String),
    #[error("unsupported {feature}: {reason}")]
    Unsupported { feature: String, reason: String },
    #[error("ESC/I protocol: {0}")]
    Protocol(String),
    #[error("scan cancelled")]
    Cancelled,
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("invalid settings: {0}")]
    Invalid(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TIFF: {0}")]
    Tiff(#[from] tiff::TiffError),
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn unsupported(feature: impl Into<String>, reason: impl Into<String>) -> Error {
    Error::Unsupported {
        feature: feature.into(),
        reason: reason.into(),
    }
}
