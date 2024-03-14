use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Mongodb error: {0}")]
    IO(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
