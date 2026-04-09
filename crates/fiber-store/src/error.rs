use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("Database internal error: {0}")]
    DBInternalError(String),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("Backup error: {0}")]
    BackupError(String),

    #[error("Restore error: {0}")]
    RestoreError(String),
}
