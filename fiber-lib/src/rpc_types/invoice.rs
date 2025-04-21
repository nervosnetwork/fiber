use crate::fiber::config::MIN_TLC_EXPIRY_DELTA;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::{Hash256, Privkey};
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use crate::FiberConfig;
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::time::Duration;
use tentacle::secio::SecioKeyPair;

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    pub currency: Currency,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
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
}

#[derive(Clone, Serialize, Deserialize)]
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
