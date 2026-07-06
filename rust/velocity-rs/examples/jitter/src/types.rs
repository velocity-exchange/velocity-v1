use thiserror::Error;
use velocity_rs::types::SdkError;

pub type JitResult<T> = Result<T, JitError>;

#[derive(Debug, Error)]
pub enum JitError {
    #[error("{0}")]
    Program(String),
    #[error("{0}")]
    Sdk(String),
}

impl From<velocity_rs::velocity_idl::errors::ErrorCode> for JitError {
    fn from(error: velocity_rs::velocity_idl::errors::ErrorCode) -> Self {
        JitError::Program(error.to_string())
    }
}

impl From<SdkError> for JitError {
    fn from(error: SdkError) -> Self {
        JitError::Sdk(error.to_string())
    }
}
