use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("Database internal error: {0}")]
    DBInternalError(String),
}
