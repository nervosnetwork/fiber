//! Invoice types for the Fiber Network JSON-RPC API.

#[cfg(feature = "cli")]
use fiber_cli_derive::CliArgs;

use crate::schema_helpers::*;
use crate::serde_utils::{duration_hex, Hash256, Pubkey, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// The currency of the invoice, can also used to represent the CKB network chain.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
pub enum Currency {
    /// The mainnet currency of CKB.
    Fibb,
    /// The testnet currency of the CKB network.
    Fibt,
    /// The devnet currency of the CKB network.
    #[default]
    Fibd,
}

/// HashAlgorithm is the hash algorithm used in the hash lock.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default, Hash, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    /// The default hash algorithm, CkbHash
    #[default]
    CkbHash,
    /// The sha256 hash algorithm
    Sha256,
}

/// The status of an invoice.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum CkbInvoiceStatus {
    /// The invoice is open and can be paid.
    Open,
    /// The invoice is cancelled.
    Cancelled,
    /// The invoice is expired.
    Expired,
    /// The invoice is received, but not settled yet.
    Received,
    /// The invoice is paid.
    Paid,
}

/// The attributes of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Attribute {
    #[serde(with = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// This attribute is deprecated since v0.6.0, The final tlc time out, in milliseconds
    FinalHtlcTimeout(u64),
    #[serde(with = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// The final tlc minimum expiry delta, in milliseconds, default is 1 day
    FinalHtlcMinimumExpiryDelta(u64),
    #[serde(with = "duration_hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// The expiry time of the invoice, in seconds
    ExpiryTime(std::time::Duration),
    /// The description of the invoice
    Description(String),
    /// The fallback address of the invoice
    FallbackAddr(String),
    /// The udt type script of the invoice (serialized as 0x-prefixed hex of molecule bytes)
    UdtScript(String),
    /// The payee public key of the invoice (validated compressed secp256k1 key, hex without 0x prefix)
    PayeePublicKey(Pubkey),
    /// The hash algorithm of the invoice
    HashAlgorithm(HashAlgorithm),
    /// The feature flags of the invoice
    Feature(Vec<String>),
    /// The payment secret of the invoice
    PaymentSecret(String),
}

/// The metadata of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InvoiceData {
    /// The timestamp of the invoice
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u128,
    /// The payment hash of the invoice
    pub payment_hash: Hash256,
    /// The attributes of the invoice, e.g. description, expiry time, etc.
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice.
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
///
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CkbInvoice {
    /// The currency of the invoice
    pub currency: Currency,
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    /// The amount of the invoice
    pub amount: Option<u128>,
    /// The signature of the invoice (hex encoded)
    pub signature: Option<String>,
    /// The invoice data, including the payment hash, timestamp and other attributes
    pub data: InvoiceData,
}

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize, Default, Clone, JsonSchema)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    #[cfg_attr(feature = "cli", cli(serde_enum))]
    pub currency: Currency,
    /// The preimage to settle an incoming TLC payable to this invoice. If preimage is set, hash must be absent.
    /// If both preimage and hash are absent, a random preimage is generated.
    pub payment_preimage: Option<Hash256>,
    /// The hash of the preimage. If hash is set, preimage must be absent. This condition indicates a 'hold invoice'
    /// for which the tlc must be accepted and held until the preimage becomes known.
    pub payment_hash: Option<Hash256>,
    /// The expiry time of the invoice, in seconds.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub expiry: Option<u64>,
    /// The fallback address of the invoice.
    pub fallback_address: Option<String>,
    /// The final HTLC timeout of the invoice, in milliseconds.
    /// Minimal value is 16 hours, and maximal value is 14 days.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub final_expiry_delta: Option<u64>,
    /// The UDT type script of the invoice.
    #[cfg_attr(feature = "cli", cli(json))]
    pub udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    #[cfg_attr(feature = "cli", cli(serde_enum))]
    pub hash_algorithm: Option<HashAlgorithm>,
    /// Whether allow payment to use MPP
    #[cfg_attr(feature = "cli", cli(bool_flag, default = false))]
    pub allow_mpp: Option<bool>,
    /// Whether allow payment to use trampoline routing
    #[cfg_attr(feature = "cli", cli(bool_flag, default = false))]
    pub allow_trampoline_routing: Option<bool>,
}

/// Result of creating a new invoice.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct InvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
}

/// Parameters for parsing an invoice.
#[derive(Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct ParseInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

/// Result of parsing an invoice.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ParseInvoiceResult {
    /// The invoice.
    pub invoice: CkbInvoice,
}

/// Parameters for getting an invoice by payment hash.
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct InvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
}

/// Parameters for settling an invoice.
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct SettleInvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
}

/// Result of settling an invoice.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SettleInvoiceResult {}

/// The status of the invoice.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetInvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
    /// The invoice status
    pub status: CkbInvoiceStatus,
}
