use ckb_types::packed::Script;

use super::payment::{SendPaymentData, SendPaymentDataBuilder};
use fiber_types::{Hash256, HashAlgorithm, PrevTlcInfo, Pubkey, TrampolineContext};

/// A validated trampoline forwarding request ready for immediate dispatch.
///
/// Keeping this separate from `PaymentActor` construction gives the hosted LSP
/// integration a narrow point where it can later choose to defer delivery.
#[derive(Clone, Debug)]
pub(crate) struct TrampolineForwardingRequest {
    pub payment_hash: Hash256,
    pub next_node_id: Pubkey,
    pub amount_to_forward: u128,
    pub hash_algorithm: HashAlgorithm,
    pub build_max_fee_amount: u128,
    pub tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub max_parts: Option<u64>,
    pub udt_type_script: Option<Script>,
    pub remaining_trampoline_onion: Vec<u8>,
    pub previous_tlc: PrevTlcInfo,
    pub max_outgoing_tlc_expiry: u64,
}

impl TrampolineForwardingRequest {
    pub(crate) fn into_send_payment_data(self) -> Result<SendPaymentData, String> {
        SendPaymentDataBuilder::new(self.next_node_id, self.amount_to_forward, self.payment_hash)
            .final_tlc_expiry_delta(self.tlc_expiry_delta)
            .tlc_expiry_limit(self.tlc_expiry_limit)
            .max_fee_amount(Some(self.build_max_fee_amount))
            .max_parts(self.max_parts)
            .udt_type_script(self.udt_type_script)
            .trampoline_context(Some(TrampolineContext {
                remaining_trampoline_onion: self.remaining_trampoline_onion,
                // The current trampoline forwarding flow supports one upstream TLC.
                previous_tlcs: vec![self.previous_tlc],
                hash_algorithm: self.hash_algorithm,
                max_outgoing_tlc_expiry: Some(self.max_outgoing_tlc_expiry),
            }))
            .allow_mpp(self.max_parts.is_some_and(|value| value > 1))
            .build()
    }
}
