use std::fmt::Display;
use std::num::ParseIntError;
use thiserror::Error;

#[derive(Error, Debug)]
pub struct VerificationError(pub molecule::error::VerificationError);

impl PartialEq for VerificationError {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
impl Display for VerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Error, PartialEq, Debug)]
pub enum InvoiceError {
    #[error("Bech32 error: {0}")]
    Bech32Error(bech32::Error),
    #[error("Molecule error: {0}")]
    MoleculeError(VerificationError),
    #[error("Failed to parse amount: {0}")]
    ParseAmountError(ParseIntError),
    #[error("Unknown currency: {0}")]
    UnknownCurrency(String),
    #[error("Unknown si prefix: {0}")]
    UnknownSiPrefix(String),
    #[error("Parsing failed with malformed HRP: {0}")]
    MalformedHRP(String),
    #[error("Too short data part")]
    TooShortDataPart,
    #[error("Unexpected end of tagged fields")]
    UnexpectedEndOfTaggedFields,
    #[error("Integer overflow error")]
    IntegerOverflowError,
    #[error("Invalid recovery id")]
    InvalidRecoveryId,
    #[error("Invalid slice length: {0}")]
    InvalidSliceLength(String),
    #[error("Invalid signature")]
    InvalidSignature,
    /// Duplicated attribute key
    #[error("Duplicated attribute key: {0}")]
    DuplicatedAttributeKey(String),
    /// Both set payment_hash and payment_preimage
    #[error("Both payment_hash and payment_preimage are set")]
    BothPaymenthashAndPreimage,
    /// An error occurred during signing
    #[error("Sign error")]
    SignError,
    #[error("Hex decode error: {0}")]
    HexDecodeError(#[from] hex::FromHexError),
    #[error("Duplicated invoice found: {0}")]
    DuplicatedInvoice(String),
    #[error("Description with length of {0} is too long, max length is 639")]
    DescriptionTooLong(usize),
    #[error("Invoice not found")]
    InvoiceNotFound,
}
