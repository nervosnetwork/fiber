use crate::invoice::CkbInvoiceStatus;
use crate::FiberConfig;
use fiber_types::{CkbInvoice, Currency, Hash256, HashAlgorithm, Pubkey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;

pub const X402_VERSION: u32 = 2;
pub const X402_SCHEME_EXACT: &str = "exact";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    pub scheme: String,
    pub network: String,
    pub asset: String,
    pub amount: String,
    pub pay_to: String,
    pub max_timeout_seconds: u64,
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    pub x402_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceInfo>,
    pub accepted: PaymentRequirements,
    pub payload: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub x402_version: u32,
    pub payment_payload: PaymentPayload,
    pub payment_requirements: PaymentRequirements,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    pub is_valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SettleRequest {
    pub x402_version: u32,
    pub payment_payload: PaymentPayload,
    pub payment_requirements: PaymentRequirements,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SettleResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    pub transaction: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SupportedKind {
    pub x402_version: u32,
    pub scheme: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SupportedResponse {
    pub kinds: Vec<SupportedKind>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub signers: HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiberExactProof {
    pub invoice: String,
    pub payment_preimage: Hash256,
}

impl FiberExactProof {
    pub fn from_payload(payload: &HashMap<String, Value>) -> Result<Self, String> {
        let invoice = payload
            .get("invoice")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing_invoice".to_string())?
            .to_string();
        let payment_preimage = payload
            .get("paymentPreimage")
            .or_else(|| payload.get("payment_preimage"))
            .and_then(Value::as_str)
            .ok_or_else(|| "missing_payment_preimage".to_string())?;

        let payment_preimage = Hash256::from_str(payment_preimage)
            .map_err(|_| "invalid_payment_preimage".to_string())?;

        Ok(Self {
            invoice,
            payment_preimage,
        })
    }
}

pub fn x402_network(config: &FiberConfig) -> String {
    match config.chain.as_str() {
        "mainnet" => "fiber:mainnet".to_string(),
        "testnet" => "fiber:testnet".to_string(),
        _ => "fiber:dev".to_string(),
    }
}

pub fn x402_asset(currency: Currency) -> &'static str {
    match currency {
        Currency::Fibb | Currency::Fibt | Currency::Fibd => "ckb",
    }
}

pub fn x402_asset_for_invoice(invoice: &CkbInvoice) -> &'static str {
    x402_asset(invoice.currency)
}

pub fn invoice_payee(invoice: &CkbInvoice) -> Result<Pubkey, String> {
    if let Some(payee) = invoice.payee_pub_key() {
        return Ok((*payee).into());
    }

    invoice
        .recover_payee_pub_key()
        .map(Into::into)
        .map_err(|_| "missing_payee_pubkey".to_string())
}

pub fn invoice_hash_algorithm(invoice: &CkbInvoice) -> HashAlgorithm {
    invoice.hash_algorithm().copied().unwrap_or_default()
}

pub fn verify_invoice_preimage(invoice: &CkbInvoice, payment_preimage: Hash256) -> bool {
    Hash256::from(invoice_hash_algorithm(invoice).hash(payment_preimage.as_ref()))
        == *invoice.payment_hash()
}

pub fn is_invoice_paid(status: CkbInvoiceStatus) -> bool {
    status == CkbInvoiceStatus::Paid
}
