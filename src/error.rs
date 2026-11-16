use thiserror::Error;

/// Library-wide error type. Provider-specific details remain available in the
/// source field without leaking a concrete HTTP client into the public API.
#[derive(Debug, Error)]
pub enum Error {
    #[error("provider {provider} failed: {message}")]
    Provider { provider: String, message: String },
    #[error("provider {0} does not support this capability")]
    Unsupported(String),
    #[error("resource not found")]
    NotFound,
    #[error("request timed out")]
    Timeout,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
