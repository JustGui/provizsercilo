use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("No candidates available")]
    NoCandidates,
    #[error("Key resolution failed: {0}")]
    KeyResolution(String),
}
