//! Invoice-related types for the Fiber Network Node RPC API.

use crate::serde_utils::{duration_hex, U128Hex, U64Hex};
use crate::Hash256;

use ckb_jsonrpc_types::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::time::Duration;

pub use fiber_types::invoice::{
    CkbInvoiceStatus, CkbScript, Currency, HashAlgorithm, InvoiceSignature, UnknownCurrencyError,
    UnknownHashAlgorithmError,
};

// ============================================================
// RPC Attribute / InvoiceData / CkbInvoice (RPC versions)
// ============================================================

/// The attributes of the invoice (RPC version).
/// Note: `Feature` uses `Vec<String>` instead of `FeatureVector` for JSON compatibility.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribute {
    #[serde(with = "U64Hex")]
    /// This attribute is deprecated since v0.6.0, The final tlc time out, in milliseconds
    FinalHtlcTimeout(u64),
    #[serde(with = "U64Hex")]
    /// The final tlc minimum expiry delta, in milliseconds, default is 1 day
    FinalHtlcMinimumExpiryDelta(u64),
    #[serde(with = "duration_hex")]
    /// The expiry time of the invoice, in seconds
    ExpiryTime(Duration),
    /// The description of the invoice
    Description(String),
    /// The fallback address of the invoice
    FallbackAddr(String),
    /// The udt type script of the invoice
    UdtScript(CkbScript),
    /// The payee public key of the invoice
    PayeePublicKey(secp256k1::PublicKey),
    /// The hash algorithm of the invoice
    HashAlgorithm(HashAlgorithm),
    /// The feature flags of the invoice
    Feature(Vec<String>),
    /// The payment secret of the invoice
    PaymentSecret(Hash256),
}

/// The metadata of the invoice (RPC version).
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvoiceData {
    /// The timestamp of the invoice
    #[serde_as(as = "U128Hex")]
    pub timestamp: u128,
    /// The payment hash of the invoice
    pub payment_hash: Hash256,
    /// The attributes of the invoice, e.g. description, expiry time, etc.
    pub attrs: Vec<Attribute>,
}

/// Represents a CKB invoice for the RPC layer.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbInvoice {
    /// The currency of the invoice
    pub currency: Currency,
    #[serde_as(as = "Option<U128Hex>")]
    /// The amount of the invoice
    pub amount: Option<u128>,
    /// The signature of the invoice
    pub signature: Option<InvoiceSignature>,
    /// The invoice data, including the payment hash, timestamp and other attributes
    pub data: InvoiceData,
}

// ============================================================
// RPC param/result types for invoice module
// ============================================================

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    pub currency: Currency,
    /// The preimage to settle an incoming TLC payable to this invoice.
    pub payment_preimage: Option<Hash256>,
    /// The hash of the preimage.
    pub payment_hash: Option<Hash256>,
    /// The expiry time of the invoice, in seconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub expiry: Option<u64>,
    /// The fallback address of the invoice.
    pub fallback_address: Option<String>,
    /// The final HTLC timeout of the invoice, in milliseconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub final_expiry_delta: Option<u64>,
    /// The UDT type script of the invoice.
    pub udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    pub hash_algorithm: Option<HashAlgorithm>,
    /// Whether allow payment to use MPP
    pub allow_mpp: Option<bool>,
    /// Whether allow payment to use trampoline routing
    pub allow_trampoline_routing: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct InvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ParseInvoiceResult {
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SettleInvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SettleInvoiceResult {}

/// The status of the invoice.
#[derive(Clone, Serialize, Deserialize)]
pub struct GetInvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
    /// The invoice status
    pub status: CkbInvoiceStatus,
}
