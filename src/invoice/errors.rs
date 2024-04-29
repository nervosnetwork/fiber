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
impl Eq for VerificationError {}
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
    #[error("Unknown prefix")]
    BadPrefix,
    #[error("Unknown currency")]
    UnknownCurrency,
    #[error("Unknown si prefix")]
    UnknownSiPrefix,
    #[error("Malformed HRP")]
    MalformedHRP,
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
    /// No payment hash
    #[error("No payment hash")]
    NoPaymentHash,
    /// Both set payment_hash and payment_preimage
    #[error("Both payment_hash and payment_preimage are set")]
    BothPaymenthashAndPreimage,
    /// An error occurred during signing
    #[error("Sign error")]
    SignError,
    #[error("Hex decode error: {0}")]
    HexDecodeError(#[from] hex::FromHexError),
    #[error("Invoice DB error: {0:?}")]
    DuplicatedInvoice(String),
}
