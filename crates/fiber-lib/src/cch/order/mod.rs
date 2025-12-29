mod order_store;
pub(crate) mod state_machine;
mod status;

pub use order_store::CchOrderStore;
pub use state_machine::CchOrderStateMachine;
pub use status::CchOrderStatus;

use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::{
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::CkbInvoice,
};

/// The generated proxy invoice for the incoming payment.
///
/// The JSON representation:
///
/// ```text
/// { "Fiber": String } | { "Lightning": String }
/// ```
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// Fiber invoice that once paid, the hub will send the outgoing payment to Lightning
    Fiber(#[serde_as(as = "DisplayFromStr")] CkbInvoice),
    /// Lightning invoice that once paid, the hub will send the outgoing payment to Fiber
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,

    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub outgoing_pay_req: String,
    pub incoming_invoice: CchInvoice,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,

    /// Amount required to pay in Satoshis via BTC or wrapped BTC, including the fee for the cross-chain hub
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,

    pub failure_reason: Option<String>,
}

impl CchOrder {
    pub fn is_final(&self) -> bool {
        self.status == CchOrderStatus::Succeeded || self.status == CchOrderStatus::Failed
    }
}
