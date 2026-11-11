use std::collections::HashSet;
use std::sync::Arc;

use super::network::{SendOnionPacketCommand, SendPaymentResponse, ASSUME_NETWORK_MYSELF_ALIVE};
use super::types::{validate_payment_custom_records_size, BroadcastMessageWithTimestamp};
use crate::fiber::channel::{ChannelActorStateStore, ProcessingChannelError};
use crate::fiber::config::{
    DEFAULT_FINAL_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA,
    PAYMENT_MAX_PARTS_LIMIT,
};
use crate::fiber::fee::{calculate_fee_with_base, calculate_tlc_forward_fee};
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{
    GraphChannelStat, NetworkGraph, NetworkGraphStateStore, PathFindError, SendPaymentState,
};
use crate::fiber::network::{
    NetworkActorStateStore, OnChainTlcRemoveCompletion, DEFAULT_CHAIN_ACTOR_TIMEOUT,
    DEFAULT_PAYMENT_TRY_LIMIT,
};
use crate::fiber::{
    KeyPair, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::invoice::{CkbInvoice, Currency, InvoiceError, InvoiceStore, PreimageStore};
use crate::Error;
use crate::{debug_event, now_timestamp_as_millis_u64};
use ckb_hash::blake2b_256;
use ckb_types::packed::{OutPoint, Script};
pub use fiber_types::PaymentSession;
pub use fiber_types::SendPaymentData;
use fiber_types::{
    Attempt, AttemptStatus, BasicMppPaymentData, EntityHex, Hash256, HashAlgorithm, HopHint,
    PaymentCustomRecords, PaymentHopData, PaymentStatus, PeeledPaymentOnionPacket, Privkey, Pubkey,
    RemoveTlcReason, RouterHop, TlcErr, TlcErrData, TlcErrPacket, TlcErrorCode, TrampolineContext,
    DEFAULT_MAX_PARTS, DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT, USER_CUSTOM_RECORDS_MAX_INDEX,
};
use ractor::{call_t, Actor, ActorProcessingErr};
use ractor::{concurrency::Duration, ActorRef, RpcReplyPort};
use rand::Rng;
use secp256k1::Secp256k1;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use strum::AsRefStr;
use tokio::sync::RwLock;
use tracing::{debug, error, instrument, warn};

// Maximum number of trampoline nodes encoded in the inner trampoline onion.
// This is a safety guard against excessive route construction work.
const MAX_TRAMPOLINE_HOPS_LIMIT: u16 = 5;
const DEFAULT_MAX_FEE_RATE: u64 = 5;
pub(crate) const MAX_FEE_RATE_DENOMINATOR: u128 = 1000;

fn check_trampoline_outgoing_tlc_expiry(
    session: &mut PaymentSession,
    hops: &[PaymentHopData],
) -> Result<(), Error> {
    let Some(max_expiry) = session
        .request
        .trampoline_context
        .as_ref()
        .and_then(|context| context.max_outgoing_tlc_expiry)
    else {
        return Ok(());
    };

    let Some(first_hop) = hops.first() else {
        session.last_error_code = Some(TlcErrorCode::IncorrectTlcExpiry);
        return Err(Error::SendPaymentError(
            "Trampoline forwarding requires at least one outgoing hop".to_string(),
        ));
    };

    if first_hop.expiry > max_expiry {
        session.last_error_code = Some(TlcErrorCode::IncorrectTlcExpiry);
        error!(
            "trampoline outgoing tlc expiry {} exceeds upstream budget {}",
            first_hop.expiry, max_expiry
        );
        return Err(Error::SendPaymentError(
            "Trampoline outgoing TLC expiry exceeds upstream expiry budget".to_string(),
        ));
    }

    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct SendPaymentDataBuilder {
    target_pubkey: Option<Pubkey>,
    amount: Option<u128>,
    payment_hash: Option<Hash256>,
    invoice: Option<String>,
    final_tlc_expiry_delta: u64,
    tlc_expiry_limit: u64,
    timeout: Option<u64>,
    max_fee_amount: Option<u128>,
    max_parts: Option<u64>,
    keysend: bool,
    udt_type_script: Option<Script>,
    preimage: Option<Hash256>,
    custom_records: Option<PaymentCustomRecords>,
    allow_self_payment: bool,
    hop_hints: Vec<HopHint>,
    router: Vec<RouterHop>,
    allow_mpp: bool,
    dry_run: bool,
    trampoline_hops: Option<Vec<Pubkey>>,
    trampoline_context: Option<TrampolineContext>,
}

impl SendPaymentDataBuilder {
    pub fn new(target_pubkey: Pubkey, amount: u128, payment_hash: Hash256) -> Self {
        Self {
            target_pubkey: Some(target_pubkey),
            amount: Some(amount),
            payment_hash: Some(payment_hash),
            // Match the defaults used by SendPaymentData::new.
            final_tlc_expiry_delta: DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            ..Default::default()
        }
    }

    pub fn target_pubkey(mut self, target_pubkey: Pubkey) -> Self {
        self.target_pubkey = Some(target_pubkey);
        self
    }

    pub fn amount(mut self, amount: u128) -> Self {
        self.amount = Some(amount);
        self
    }

    pub fn payment_hash(mut self, payment_hash: Hash256) -> Self {
        self.payment_hash = Some(payment_hash);
        self
    }

    pub fn invoice(mut self, invoice: Option<String>) -> Self {
        self.invoice = invoice;
        self
    }

    pub fn final_tlc_expiry_delta(mut self, final_tlc_expiry_delta: u64) -> Self {
        self.final_tlc_expiry_delta = final_tlc_expiry_delta;
        self
    }

    pub fn tlc_expiry_limit(mut self, tlc_expiry_limit: u64) -> Self {
        self.tlc_expiry_limit = tlc_expiry_limit;
        self
    }

    pub fn timeout(mut self, timeout: Option<u64>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn max_fee_amount(mut self, max_fee_amount: Option<u128>) -> Self {
        self.max_fee_amount = max_fee_amount;
        self
    }

    pub fn max_parts(mut self, max_parts: Option<u64>) -> Self {
        self.max_parts = max_parts;
        self
    }

    pub fn keysend(mut self, keysend: bool) -> Self {
        self.keysend = keysend;
        self
    }

    pub fn udt_type_script(mut self, udt_type_script: Option<Script>) -> Self {
        self.udt_type_script = udt_type_script;
        self
    }

    pub fn preimage(mut self, preimage: Option<Hash256>) -> Self {
        self.preimage = preimage;
        self
    }

    pub fn custom_records(mut self, custom_records: Option<PaymentCustomRecords>) -> Self {
        self.custom_records = custom_records;
        self
    }

    pub fn allow_self_payment(mut self, allow_self_payment: bool) -> Self {
        self.allow_self_payment = allow_self_payment;
        self
    }

    pub fn hop_hints(mut self, hop_hints: Vec<HopHint>) -> Self {
        self.hop_hints = hop_hints;
        self
    }

    pub fn router(mut self, router: Vec<RouterHop>) -> Self {
        self.router = router;
        self
    }

    pub fn allow_mpp(mut self, allow_mpp: bool) -> Self {
        self.allow_mpp = allow_mpp;
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn trampoline_hops(mut self, trampoline_hops: Option<Vec<Pubkey>>) -> Self {
        self.trampoline_hops = trampoline_hops;
        self
    }

    pub fn trampoline_context(mut self, trampoline_context: Option<TrampolineContext>) -> Self {
        self.trampoline_context = trampoline_context;
        self
    }

    fn validate(&self, target_pubkey: Pubkey, amount: u128) -> Result<(), String> {
        if amount == 0 {
            return Err("amount must be greater than 0".to_string());
        }

        if amount
            .checked_add(self.max_fee_amount.expect("must got max_fee_amount"))
            .is_none()
        {
            return Err(format!(
                "amount + max_fee_amount overflow: amount = {}, max_fee_amount = {:?}",
                amount, self.max_fee_amount
            ));
        }

        if !(MIN_TLC_EXPIRY_DELTA..=MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .contains(&self.final_tlc_expiry_delta)
        {
            return Err(format!(
                "invalid final_tlc_expiry_delta, expect between {} and {}",
                MIN_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT
            ));
        }

        if self.tlc_expiry_limit < self.final_tlc_expiry_delta
            || self.tlc_expiry_limit < MIN_TLC_EXPIRY_DELTA
        {
            return Err(format!(
                "tlc_expiry_limit is too small, final_tlc_expiry_delta: {}, tlc_expiry_limit: {}",
                self.final_tlc_expiry_delta, self.tlc_expiry_limit
            ));
        }
        if self.tlc_expiry_limit > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            return Err(format!(
                "tlc_expiry_limit is too large, expect it to less than {}",
                MAX_PAYMENT_TLC_EXPIRY_LIMIT
            ));
        }

        if let Some(hops) = self.trampoline_hops.as_ref() {
            if hops.is_empty() {
                return Err("trampoline_hops must be non-empty when provided".to_string());
            }
            if hops.len() > MAX_TRAMPOLINE_HOPS_LIMIT as usize {
                return Err(format!(
                    "too many trampoline_hops, at most {}",
                    MAX_TRAMPOLINE_HOPS_LIMIT
                ));
            }
            if self.allow_mpp && self.max_parts.unwrap_or(DEFAULT_MAX_PARTS) > 1 && hops.len() > 1 {
                return Err(
                    "trampoline_hops must contain only one hop when MPP is enabled".to_string(),
                );
            }
            if hops.contains(&target_pubkey) {
                return Err("trampoline_hops must not contain target_pubkey".to_string());
            }
            if self.max_fee_amount.is_none() {
                return Err(
                    "max_fee_amount must be set when trampoline_hops is provided".to_string(),
                );
            }
            if hops.iter().collect::<HashSet<_>>().len() != hops.len() {
                return Err("trampoline_hops must not contain duplicates".to_string());
            }
            if self.allow_self_payment {
                return Err(
                    "allow_self_payment cannot be enabled with trampoline routing".to_string(),
                );
            }
        }

        if self.udt_type_script.is_none() && amount >= u64::MAX as u128 {
            return Err(format!(
                "The payment amount ({}) should be less than {}",
                amount,
                u64::MAX
            ));
        }

        if let Some(custom_records) = &self.custom_records {
            validate_payment_custom_records_size(custom_records)?;
        }

        if let Some(max_parts) = self.max_parts {
            if max_parts == 0 || max_parts > PAYMENT_MAX_PARTS_LIMIT {
                return Err(format!(
                    "invalid max_parts, value should be in range [1, {}]",
                    PAYMENT_MAX_PARTS_LIMIT
                ));
            }
            if self.allow_mpp && max_parts <= 1 {
                return Err(format!(
                    "invalid max_parts, value should be in range [1, {}]",
                    PAYMENT_MAX_PARTS_LIMIT
                ));
            }
            if !self.allow_mpp && max_parts > 1 && self.trampoline_context.is_none() {
                return Err("payment invoice is not allow MPP".to_string());
            }
        }

        if self.keysend {
            if self.invoice.is_some() {
                return Err("keysend payment should not have invoice".to_string());
            }
            if self.preimage.is_none() {
                return Err("keysend payment should have preimage".to_string());
            }
        }

        Ok(())
    }

    pub fn build(self) -> Result<SendPaymentData, String> {
        let target_pubkey = self
            .target_pubkey
            .ok_or_else(|| "target_pubkey is missing".to_string())?;
        let amount = self.amount.ok_or_else(|| "amount is missing".to_string())?;
        let payment_hash = self
            .payment_hash
            .ok_or_else(|| "payment_hash is missing".to_string())?;

        self.validate(target_pubkey, amount)?;

        Ok(SendPaymentData {
            target_pubkey,
            amount,
            payment_hash,
            invoice: self.invoice,
            final_tlc_expiry_delta: self.final_tlc_expiry_delta,
            tlc_expiry_limit: self.tlc_expiry_limit,
            timeout: self.timeout,
            max_fee_amount: self.max_fee_amount,
            max_parts: self.max_parts,
            keysend: self.keysend,
            udt_type_script: self.udt_type_script,
            preimage: self.preimage,
            custom_records: self.custom_records,
            allow_self_payment: self.allow_self_payment,
            hop_hints: self.hop_hints,
            router: self.router,
            allow_mpp: self.allow_mpp,
            dry_run: self.dry_run,
            trampoline_hops: self.trampoline_hops,
            trampoline_context: self.trampoline_context,
        })
    }
}

pub trait SendPaymentDataExt {
    /// Validates a payment command against the payer node's configured currency.
    fn new(command: SendPaymentCommand, currency: Currency) -> Result<SendPaymentData, String>;
    fn trampoline_hops(&self) -> Option<&[Pubkey]>;
    fn payment_invoice(&self) -> Option<CkbInvoice>;
    fn hash_algorithm(&self) -> HashAlgorithm;
}

impl SendPaymentDataExt for SendPaymentData {
    fn new(command: SendPaymentCommand, currency: Currency) -> Result<SendPaymentData, String> {
        // `from_str` requires the invoice to carry a valid signature, so parsing
        // here also guarantees the invoice is signed.
        let invoice = command
            .invoice
            .as_ref()
            .map(|invoice| invoice.parse::<CkbInvoice>())
            .transpose()
            .map_err(|err| match err {
                InvoiceError::MissingSignature => "invoice is not signed".to_string(),
                _ => "invoice is invalid".to_string(),
            })?;

        if let Some(invoice) = invoice.as_ref() {
            if invoice.currency != currency {
                return Err(format!(
                    "invoice currency {} does not match node currency {}",
                    invoice.currency, currency
                ));
            }
            if invoice.is_expired() {
                return Err("invoice is expired".to_string());
            }
        }

        fn validate_field<T: PartialEq + Clone>(
            field: Option<T>,
            invoice_field: Option<T>,
            field_name: &str,
        ) -> Result<T, String> {
            match (field, invoice_field) {
                (Some(f), Some(i)) => {
                    if f != i {
                        return Err(format!("{} does not match the invoice", field_name));
                    }
                    Ok(f)
                }
                (Some(f), None) => Ok(f),
                (None, Some(i)) => Ok(i),
                (None, None) => Err(format!("{} is missing", field_name)),
            }
        }

        let target = validate_field(
            command.target_pubkey,
            invoice
                .as_ref()
                .and_then(|i| i.payee_pub_key().cloned().map(Pubkey::from)),
            "target_pubkey",
        )?;

        let amount = validate_field(
            command.amount,
            invoice.as_ref().and_then(|i| i.amount()),
            "amount",
        )?;

        let max_fee_rate = command.max_fee_rate.unwrap_or(DEFAULT_MAX_FEE_RATE) as u128;
        let max_fee_amount_by_rate =
            calculate_fee_with_base(amount, max_fee_rate, MAX_FEE_RATE_DENOMINATOR)?;

        let max_fee_amount = match command.max_fee_amount {
            Some(max_fee_amount) => Some(max_fee_amount.min(max_fee_amount_by_rate)),
            None => Some(max_fee_amount_by_rate),
        };

        let udt_type_script = match validate_field(
            command.udt_type_script.clone(),
            invoice.as_ref().and_then(|i| i.udt_type_script().cloned()),
            "udt_type_script",
        ) {
            Ok(script) => Some(script),
            Err(e) if e == "udt_type_script is missing" => None,
            Err(e) => return Err(e),
        };

        // check htlc expiry delta and limit are both valid if it is set
        let final_tlc_expiry_delta = match invoice.as_ref() {
            Some(invoice) => match invoice.final_tlc_minimum_expiry_delta().copied() {
                Some(delta) => delta,
                None => {
                    let minimum_delta = invoice.final_tlc_minimum_expiry_delta_or_default();
                    match command.final_tlc_expiry_delta {
                        Some(delta) if delta < minimum_delta => {
                            return Err(format!(
                                "final_tlc_expiry_delta is below the invoice minimum: {} < {}",
                                delta, minimum_delta
                            ));
                        }
                        Some(delta) => delta,
                        None => minimum_delta,
                    }
                }
            },
            None => command
                .final_tlc_expiry_delta
                .unwrap_or(DEFAULT_FINAL_TLC_EXPIRY_DELTA),
        };

        let tlc_expiry_limit = command
            .tlc_expiry_limit
            .unwrap_or(MAX_PAYMENT_TLC_EXPIRY_LIMIT);

        let keysend = command.keysend.unwrap_or(false);
        let (payment_hash, preimage) = if !keysend {
            (
                validate_field(
                    command.payment_hash,
                    invoice.as_ref().map(|i| *i.payment_hash()),
                    "payment_hash",
                )?,
                None,
            )
        } else {
            if invoice.is_some() {
                return Err("keysend payment should not have invoice".to_string());
            }
            if command.payment_hash.is_some() {
                return Err("keysend payment should not have payment_hash".to_string());
            }
            // generate a random preimage for keysend payment
            let mut rng = rand::rng();
            let mut result = [0u8; 32];
            rng.fill(&mut result[..]);
            let preimage: Hash256 = result.into();
            // use the default payment hash algorithm here for keysend payment
            let payment_hash: Hash256 = blake2b_256(preimage).into();
            (payment_hash, Some(preimage))
        };

        let hop_hints = command.hop_hints.unwrap_or_default();
        if !hop_hints.is_empty()
            && invoice
                .as_ref()
                .is_some_and(|inv| !inv.allow_trampoline_routing())
        {
            return Err("invoice does not support hop hints".to_string());
        }

        let allow_mpp = invoice.as_ref().is_some_and(|inv| inv.allow_mpp());
        let payment_secret = invoice
            .as_ref()
            .and_then(|inv| inv.payment_secret().cloned());
        if allow_mpp && payment_secret.is_none() {
            return Err("payment secret is required for multi-path payment".to_string());
        }

        if let Some(custom_records) = &command.custom_records {
            if custom_records
                .data
                .keys()
                .any(|k| *k > USER_CUSTOM_RECORDS_MAX_INDEX)
            {
                return Err(format!(
                    "custom_records key should in range 0 ~ {:?}",
                    USER_CUSTOM_RECORDS_MAX_INDEX
                ));
            }
        }

        let mut custom_records = command.custom_records;
        // bolt04 write payment data record to custom records if payment secret is set
        if let Some(payment_secret) = payment_secret {
            let records = custom_records.get_or_insert_with(PaymentCustomRecords::default);
            BasicMppPaymentData::new(payment_secret, amount).write(records);
        }

        SendPaymentDataBuilder::new(target, amount, payment_hash)
            .invoice(command.invoice)
            .final_tlc_expiry_delta(final_tlc_expiry_delta)
            .tlc_expiry_limit(tlc_expiry_limit)
            .timeout(command.timeout)
            .max_fee_amount(max_fee_amount)
            .max_parts(command.max_parts)
            .keysend(keysend)
            .udt_type_script(udt_type_script)
            .preimage(preimage)
            .custom_records(custom_records)
            .allow_self_payment(command.allow_self_payment)
            .hop_hints(hop_hints)
            .allow_mpp(allow_mpp)
            .dry_run(command.dry_run)
            .trampoline_hops(command.trampoline_hops)
            .build()
    }

    fn trampoline_hops(&self) -> Option<&[Pubkey]> {
        self.trampoline_hops.as_deref()
    }

    fn payment_invoice(&self) -> Option<CkbInvoice> {
        self.invoice
            .as_ref()
            .and_then(|x| x.parse::<CkbInvoice>().ok())
    }

    fn hash_algorithm(&self) -> HashAlgorithm {
        let invoice = self.payment_invoice();
        invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_else(|| {
                self.trampoline_context
                    .as_ref()
                    .map(|c| c.hash_algorithm)
                    .unwrap_or_default()
            })
    }
}

pub trait PaymentSessionExt {
    fn new_session(
        store: &impl NetworkGraphStateStore,
        request: SendPaymentData,
        try_limit: u32,
    ) -> PaymentSession;
    fn init_attempts(self, store: &impl NetworkGraphStateStore) -> PaymentSession;
    fn flush_attempts(&mut self, store: &impl NetworkGraphStateStore);
    /// Whether issued TLCs or channel-owned work still need an outcome.
    /// Unissued attempts cannot prolong a final payment.
    fn has_pending_payment_work(&self, store: &impl ChannelActorStateStore) -> bool;
    /// Issued TLCs, queued Adds and current completion receipts still owned by a channel.
    fn has_outstanding_channel_work(&self, store: &impl ChannelActorStateStore) -> bool;
}

impl PaymentSessionExt for PaymentSession {
    fn has_pending_payment_work(&self, store: &impl ChannelActorStateStore) -> bool {
        self.has_outstanding_channel_work(store)
            || (!self.status.is_final()
                && !self.retry_stopped
                && self.attempts().any(|attempt| !attempt.status.is_final()))
    }

    fn has_outstanding_channel_work(&self, store: &impl ChannelActorStateStore) -> bool {
        self.attempts().any(|attempt| {
            attempt.is_inflight()
                || attempt
                    .first_hop_channel_outpoint()
                    .and_then(|outpoint| store.get_channel_state_by_outpoint(outpoint))
                    .is_some_and(|channel| {
                        channel.owns_payment_attempt(self.payment_hash(), attempt.id)
                            || channel.session_v2.as_ref().is_some_and(|session| {
                                session.remove_effects.iter().any(|(tlc, execution)| {
                                    tlc.payment_hash == self.payment_hash()
                                        && tlc.attempt_id == Some(attempt.id)
                                        && ((attempt.status == AttemptStatus::Created
                                            || execution.is_none_or(|number| {
                                                number == attempt.tried_times
                                                    || (attempt.is_retrying()
                                                        && number.saturating_add(1)
                                                            == attempt.tried_times)
                                            }))
                                            || (attempt.preimage.is_none()
                                                && matches!(
                                                    tlc.removed_reason,
                                                    Some(RemoveTlcReason::RemoveTlcFulfill(_))
                                                )))
                                })
                            })
                    })
        })
    }

    fn new_session(
        store: &impl NetworkGraphStateStore,
        request: SendPaymentData,
        try_limit: u32,
    ) -> PaymentSession {
        let now = now_timestamp_as_millis_u64();
        PaymentSession {
            request,
            last_error: None,
            last_error_code: None,
            try_limit,
            status: PaymentStatus::Created,
            created_at: now,
            last_updated_at: now,
            cached_attempts: vec![],
            retry_stopped: false,
        }
        .init_attempts(store)
    }

    fn init_attempts(mut self, store: &impl NetworkGraphStateStore) -> PaymentSession {
        self.flush_attempts(store);
        self
    }

    fn flush_attempts(&mut self, store: &impl NetworkGraphStateStore) {
        self.cached_attempts = store.get_attempts(self.request.payment_hash);
        self.status = self.calc_payment_status();
        if self.status == PaymentStatus::Success {
            self.last_error = None;
            self.last_error_code = None;
        }
    }
}

impl From<PaymentSession> for SendPaymentResponse {
    fn from(session: PaymentSession) -> Self {
        let status = session.status;
        let fee = session.fee_paid();
        let payment_preimage = session.payment_preimage();
        let mut all_attempts = session
            .attempts()
            .map(|a| {
                (
                    a.id,
                    a.status,
                    a.last_error.clone(),
                    a.tried_times,
                    a.route.receiver_amount(),
                )
            })
            .collect::<Vec<_>>();
        all_attempts.sort_by_key(|a| a.0);

        #[cfg(any(debug_assertions, test, feature = "bench"))]
        let attempts = session
            .attempts()
            .filter(|a| !a.is_failed())
            .collect::<Vec<_>>();

        Self {
            payment_hash: session.request.payment_hash,
            payment_preimage,
            status,
            failed_error: session.last_error.clone(),
            created_at: session.created_at,
            last_updated_at: session.last_updated_at,
            custom_records: session.request.custom_records.clone(),
            fee,
            #[cfg(any(debug_assertions, test, feature = "bench"))]
            routers: attempts.iter().map(|a| a.route.clone()).collect::<Vec<_>>(),
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SendPaymentCommand {
    // the identifier of the payment target
    pub target_pubkey: Option<Pubkey>,
    // the amount of the payment
    pub amount: Option<u128>,
    // The hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,
    // the encoded invoice to send to the recipient
    pub invoice: Option<String>,
    // the TLC expiry delta that should be used to set the timelock for the final hop
    pub final_tlc_expiry_delta: Option<u64>,
    // the TLC expiry for whole payment, in milliseconds
    pub tlc_expiry_limit: Option<u64>,
    // the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    pub timeout: Option<u64>,
    // the maximum fee amounts in shannons that the sender is willing to pay, default is 0.5% * amount.
    pub max_fee_amount: Option<u128>,
    // the maximum fee rate per thousand (‰), default is 5 (0.5%).
    pub max_fee_rate: Option<u64>,
    // max parts for the payment, only used for multi-part payments
    pub max_parts: Option<u64>,
    // keysend payment, default is false
    pub keysend: Option<bool>,
    // udt type script
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    // allow self payment, default is false
    pub allow_self_payment: bool,
    // custom records
    pub custom_records: Option<PaymentCustomRecords>,
    // the hop hint which may help the find path algorithm to find the path
    pub hop_hints: Option<Vec<HopHint>>,
    // dry_run only used for checking, default is false
    pub dry_run: bool,
    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,
}

impl SendPaymentCommand {
    /// Builds payment data after validating it against the payer node's currency.
    pub fn build_send_payment_data(self, currency: Currency) -> Result<SendPaymentData, Error> {
        SendPaymentData::new(self, currency).map_err(|e| {
            error!("Failed to validate payment request: {:?}", e);
            Error::InvalidParameter(format!("Failed to validate payment request: {:?}", e))
        })
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SendPaymentWithRouterCommand {
    /// the hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,

    /// The router to use for the payment
    pub router: Vec<RouterHop>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data
    /// For example:
    /// ```json
    /// "custom_records": {
    ///    "0x1": "0x01020304",
    ///    "0x2": "0x05060708",
    ///    "0x3": "0x090a0b0c",
    ///    "0x4": "0x0d0e0f10010d090a0b0c"
    ///  }
    /// ```
    pub custom_records: Option<PaymentCustomRecords>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: bool,
}

impl SendPaymentWithRouterCommand {
    /// Builds explicit-route payment data using the payer node's currency.
    pub fn build_send_payment_data(
        self,
        source: Pubkey,
        currency: Currency,
    ) -> Result<SendPaymentData, Error> {
        // Only proceed if we have at least one hop requirement
        let Some(last_edge) = self.router.last() else {
            return Err(Error::InvalidParameter(
                "No hop requirements provided".to_string(),
            ));
        };

        let target = last_edge.target;
        let amount = last_edge.amount_received;

        // Create payment command with defaults from the last hop
        let payment_command = SendPaymentCommand {
            target_pubkey: Some(target),
            payment_hash: self.payment_hash,
            invoice: self.invoice,
            allow_self_payment: target == source,
            dry_run: self.dry_run,
            amount: Some(amount),
            keysend: self.keysend,
            udt_type_script: self.udt_type_script.clone(),
            custom_records: self.custom_records,
            ..Default::default()
        };

        let mut payment_data = SendPaymentData::new(payment_command, currency).map_err(|e| {
            error!("Failed to validate payment request: {:?}", e);
            Error::InvalidParameter(format!("Failed to validate payment request: {:?}", e))
        })?;

        // specify the router to be used
        payment_data.router = self.router.clone();
        Ok(payment_data)
    }
}

/// The interval at which to check payment status for timeout detection
const PAYMENT_STATUS_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Source identity and attempt binding for acknowledging a persisted TLC removal.
#[derive(Clone, Debug)]
pub struct PaymentTlcRemoveContext {
    pub channel_id: Hash256,
    pub tlc_id: fiber_types::TLCId,
    pub channel_outpoint: OutPoint,
    pub tried_times: u32,
}

#[derive(Debug, AsRefStr)]
pub enum PaymentActorMessage {
    /// Observe a channel outcome and keep existing payment work available.
    TlcUpdated,
    // SendPayment
    SendPayment(
        SendPaymentData,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    RetrySendPayment(Option<u64>),
    /// Resume startup work unless this actor already scheduled or handled this attempt.
    ResumeAfterClosedChannel(u64),
    /// Acknowledge a closed source only after its queued Add's outcome is persisted.
    CompleteClosedLocalAdd {
        channel_id: Hash256,
        channel_outpoint: OutPoint,
        command: Box<fiber_types::AddTlcCommand>,
        reply: RpcReplyPort<Result<(), String>>,
    },
    OnAddTlcResultEvent {
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
    },
    OnRemoveTlcEvent {
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
        /// Locally detected failure; None uses the peer's onion error packet.
        local_error: Option<TlcErr>,
        /// Request an asynchronous receipt after persisting the payment outcome.
        completion: Option<PaymentTlcRemoveContext>,
    },
    /// Reconcile a first-hop attempt from a channel-scoped on-chain fulfill proof.
    ReconcileOnChainFulfill {
        attempt_id: u64,
        payment_preimage: Hash256,
        reply: RpcReplyPort<Result<(), String>>,
    },
    /// Periodic check to detect stuck payments and log status
    CheckPaymentStatus,
}

pub struct PaymentActorState {
    payment_hash: Hash256,
    init_command: Option<PaymentActorMessage>,
    last_error_packet: Option<TlcErrPacket>,

    // the number of pending retrying send payments, we track it for
    // set retry delay dynamically, pending too many payments may have a negative impact
    // on the node performance, which in worst case may lead node not response revoke_and_ack
    // in expected time, and then the peer will disconnect us.
    retry_send_payment_count: usize,
    // Deduplicate only replayed on-chain removal notifications in this actor lifetime.
    // A fresh actor must be able to recover a persisted retry whose timer was lost.
    processed_onchain_removals: HashSet<(Hash256, fiber_types::TLCId)>,
    // A fresh actor must re-arm a persisted retry, but duplicate receipts must not add timers.
    processed_closed_adds: HashSet<(Hash256, u64)>,
    // Startup must not race an outbox callback's already-scheduled retry.
    attempts_with_retry_work: HashSet<u64>,
    // Network admission only queues AddTlc in the channel mailbox. Until its
    // result arrives, the channel's persisted state may not own the attempt yet.
    // On restart those mailboxes disappear and durable channel ownership wins.
    pending_add_results: HashSet<u64>,
}

impl PaymentActorState {
    pub fn new(args: PaymentActorArguments) -> Self {
        let PaymentActorArguments {
            payment_hash,
            init_command,
        } = args;
        Self {
            payment_hash,
            init_command: Some(init_command),
            last_error_packet: None,
            retry_send_payment_count: 0,
            processed_onchain_removals: HashSet::new(),
            processed_closed_adds: HashSet::new(),
            attempts_with_retry_work: HashSet::new(),
            pending_add_results: HashSet::new(),
        }
    }
}

pub struct PaymentActorArguments {
    pub payment_hash: Hash256,
    pub init_command: PaymentActorMessage,
}

pub struct PaymentActor<S> {
    // An event emitter to notify outside observers.
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    network: ActorRef<NetworkActorMessage>,
}

#[async_trait::async_trait]
impl<S> Actor for PaymentActor<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    type Msg = PaymentActorMessage;
    type State = PaymentActorState;
    type Arguments = PaymentActorArguments;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let state = PaymentActorState::new(args);
        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // Set up periodic status check for timeout detection
        myself.send_interval(PAYMENT_STATUS_CHECK_INTERVAL, || {
            PaymentActorMessage::CheckPaymentStatus
        });

        // Recover fulfillment proof before an old failure or retry is consumed.
        self.observe_tlc_preimages(state.payment_hash);
        let init_msg = state
            .init_command
            .take()
            .ok_or_else(|| Error::InternalError(anyhow::anyhow!("No init command provided")))?;
        myself.send_message(init_msg).map_err(Into::into)
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Payment actor is stopped {:?}", myself.get_name());
        self.network
            .send_message(NetworkActorMessage::Event(
                NetworkActorEvent::PaymentActorStopped(
                    state.payment_hash,
                    state.last_error_packet.clone(),
                ),
            ))
            .map_err(Into::into)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        #[cfg(feature = "metrics")]
        let start = crate::now_timestamp_as_millis_u64();
        #[cfg(feature = "metrics")]
        let name = format!("fiber.payment_actor.{}", message.as_ref());

        if let Err(err) = self.handle_command(myself, state, message).await {
            error!("Failed to handle payment actor command: {}", err);
        }

        #[cfg(feature = "metrics")]
        {
            let end = crate::now_timestamp_as_millis_u64();
            let elapsed = end - start;
            metrics::histogram!(name).record(elapsed as u32);
        }

        Ok(())
    }
}

impl<S> PaymentActor<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        store: S,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        network: ActorRef<NetworkActorMessage>,
    ) -> Self {
        Self {
            store: store.clone(),
            network_graph,
            network,
        }
    }

    // We normally don't need to manually call this to update graph from store data,
    // because network actor will automatically update the graph when it receives
    // updates. But in some standalone tests, we may need to manually update the graph.
    async fn update_graph(&self) {
        let mut graph = self.network_graph.write().await;
        graph.load_from_store();
    }

    /// Spawn a blocking task to build route
    /// NOTE: build route is a CPU intensive task
    #[cfg(not(target_arch = "wasm32"))]
    async fn build_route_in_spawn_task(
        &self,
        amount: u128,
        amount_low_bound: Option<u128>,
        max_fee_amount: Option<u128>,
        request: SendPaymentState,
    ) -> Result<Vec<PaymentHopData>, PathFindError> {
        let network_graph = self.network_graph.clone();
        tokio::task::spawn_blocking(move || {
            let graph = network_graph.blocking_read();
            graph.build_route(amount, amount_low_bound, max_fee_amount, &request)
        })
        .await
        .map_err(|err| PathFindError::Other(format!("blocking task failed: {}", err)))?
    }

    /// NOTE: `spawn_blocking` is not supported on wasm, so this original implementation of `build_route` will be used on wasm
    #[cfg(target_arch = "wasm32")]
    async fn build_route(
        &self,
        amount: u128,
        amount_low_bound: Option<u128>,
        max_fee_amount: Option<u128>,
        request: SendPaymentState,
    ) -> Result<Vec<PaymentHopData>, PathFindError> {
        let network_graph = self.network_graph.clone();
        let graph = network_graph.read().await;
        graph.build_route(amount, amount_low_bound, max_fee_amount, &request)
    }

    #[instrument(
        skip(self, myself, state, command),
        fields(payment_hash = ?state.payment_hash)
    )]
    pub async fn handle_command(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        command: PaymentActorMessage,
    ) -> crate::Result<()> {
        let completion_actor = myself.clone();
        self.observe_tlc_preimages(state.payment_hash);
        self.advance_payment_result(state);
        match command {
            PaymentActorMessage::TlcUpdated => {
                self.check_payment_final(myself, state);
                return Ok(());
            }
            PaymentActorMessage::SendPayment(payment_request, reply) => {
                let dry_run = payment_request.dry_run;
                match self
                    .on_send_payment(myself.clone(), state, payment_request)
                    .await
                {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                        if dry_run {
                            // A dry run stores no payment session, so nothing is left to wait for.
                            myself.stop(Some("Dry run payment complete".to_string()));
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        error!("Failed to send payment: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));

                        // An admission error can occur after another MPP shard was issued.
                        // Keep that work alive until its result is known.
                        self.advance_payment_result(state);
                        if self.store.get_payment_session(state.payment_hash).is_none() {
                            myself.stop(Some("Failed to send payment".to_string()));
                        }
                    }
                }
            }
            PaymentActorMessage::ResumeAfterClosedChannel(attempt_id) => {
                if state.attempts_with_retry_work.insert(attempt_id) {
                    let _ = self
                        .resume_payment_session(myself.clone(), state, Some(attempt_id))
                        .await;
                }
            }
            PaymentActorMessage::RetrySendPayment(attempt_id) => {
                let _ = self
                    .resume_payment_session(myself.clone(), state, attempt_id)
                    .await;
            }
            PaymentActorMessage::CompleteClosedLocalAdd {
                channel_id,
                channel_outpoint,
                command,
                reply,
            } => {
                let result = self
                    .complete_closed_local_add(
                        myself.clone(),
                        state,
                        channel_id,
                        channel_outpoint,
                        &command,
                    )
                    .await;
                if result.is_ok() {
                    if let Some(id) = command.attempt_id {
                        state.pending_add_results.remove(&id);
                    }
                }
                let _ = reply.send(result);
            }
            PaymentActorMessage::OnAddTlcResultEvent {
                attempt_id,
                add_tlc_result,
            } => {
                self.handle_add_tlc_result_event(myself.clone(), state, attempt_id, add_tlc_result)
                    .await;
            }
            PaymentActorMessage::OnRemoveTlcEvent {
                attempt_id,
                reason,
                local_error,
                completion,
            } => {
                let was_success = self.store.get_persisted_payment_status(state.payment_hash)
                    == Some(PaymentStatus::Success);
                let result = self
                    .handle_remove_tlc_event(
                        state,
                        attempt_id,
                        reason.clone(),
                        local_error.as_ref(),
                        completion.as_ref(),
                    )
                    .await;
                match result {
                    Ok(retry) => {
                        if let Some(completion) = completion {
                            self.network
                                .send_message(NetworkActorMessage::new_command(
                                    NetworkActorCommand::OnChainTlcRemoveCompleted {
                                        downstream_channel_id: completion.channel_id,
                                        downstream_tlc_id: completion.tlc_id,
                                        completion: OnChainTlcRemoveCompletion::Payer,
                                        payment_hash: state.payment_hash,
                                        reason,
                                    },
                                ))
                                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                        }
                        if retry {
                            // Queue the channel acknowledgement before scheduling another try.
                            self.register_payment_retry(myself.clone(), state, attempt_id);
                        }
                    }
                    Err(err) => warn!("Failed to process TLC removal: {}", err),
                }
                if !was_success
                    && self.store.get_persisted_payment_status(state.payment_hash)
                        == Some(PaymentStatus::Success)
                {
                    self.network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::PaymentSucceeded(state.payment_hash),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
            PaymentActorMessage::ReconcileOnChainFulfill {
                attempt_id,
                payment_preimage,
                reply,
            } => {
                let result = self
                    .handle_reconcile_onchain_fulfill(state, attempt_id, payment_preimage)
                    .await
                    .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }
            PaymentActorMessage::CheckPaymentStatus => {
                self.handle_check_payment_status(myself, state);
            }
        };
        self.observe_tlc_preimages(state.payment_hash);
        self.advance_payment_result(state);
        self.check_payment_final(completion_actor, state);
        Ok(())
    }

    /// Handle periodic status check to detect stuck payments
    fn handle_check_payment_status(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
    ) {
        self.check_payment_final(myself.clone(), state);
        let payment_hash = state.payment_hash;
        if let Some(session) = self.store.get_payment_session(payment_hash) {
            if session.status.is_final() {
                return;
            } else {
                // Payment is still not final, log the current status for debugging
                let active_attempts = session.active_attempts().len();
                let inflight_attempts = session.attempts().filter(|a| a.is_inflight()).count();
                let failed_attempts = session.attempts().filter(|a| a.is_failed()).count();
                let total_attempts = session.attempts_count();

                warn!(
                    "Payment {:?} is still not final after periodic check, maybe the channel is down. \
                    Status: {:?}, Active attempts: {}, Inflight: {}, Failed: {}, Total: {}, \
                    Retry count: {}, Last error: {:?}",
                    payment_hash,
                    session.status,
                    active_attempts,
                    inflight_attempts,
                    failed_attempts,
                    total_attempts,
                    state.retry_send_payment_count,
                    session.last_error
                );

                // The channel mailbox is not durable yet. Keep its admission
                // ownership in this actor until AddTlcResult or closed-Add completion.
                if !state.pending_add_results.is_empty() {
                    return;
                }
                // The tlc may stuck due to the channel is down
                // we stop the actor, the actor will be resumed once tlc is processed
                myself.stop(Some(
                    "Payment is still not final, the tlc may stuck due to the channel is down"
                        .to_string(),
                ));
            }
        } else {
            error!(
                "Payment session not found during periodic check: {:?}",
                payment_hash
            );
            myself.stop(Some("Payment session not found".to_string()));
        }
    }

    /// Learn early fulfillment without completing the attempt's TLC lifecycle.
    fn observe_tlc_preimages(&self, payment_hash: Hash256) {
        let Some(mut session) = self.store.get_payment_session(payment_hash) else {
            return;
        };
        let was_success =
            self.store.get_persisted_payment_status(payment_hash) == Some(PaymentStatus::Success);
        let had_preimage = session.payment_preimage().is_some();
        if was_success && had_preimage {
            return;
        }
        let unobserved_attempts: Vec<_> = session
            .attempts()
            .filter(|attempt| !had_preimage && attempt.preimage.is_none())
            .cloned()
            .collect();
        for mut attempt in unobserved_attempts {
            let Some(outpoint) = attempt.first_hop_channel_outpoint() else {
                continue;
            };
            let Some(channel) = self.store.get_channel_state_by_outpoint(outpoint) else {
                continue;
            };
            let preimage = channel
                .tlc_state
                .offered_tlcs
                .tlcs
                .iter()
                .chain(
                    channel
                        .session_v2
                        .iter()
                        .flat_map(|session| session.remove_effects.iter().map(|(tlc, _)| tlc)),
                )
                .find_map(|tlc| {
                    if tlc.payment_hash != payment_hash
                        || tlc.attempt_id != Some(attempt.id)
                        || tlc.forwarding_tlc.is_some()
                    {
                        return None;
                    }
                    match &tlc.removed_reason {
                        Some(RemoveTlcReason::RemoveTlcFulfill(fulfill)) => {
                            Some(fulfill.payment_preimage)
                        }
                        _ => None,
                    }
                });
            if let Some(preimage) = preimage {
                if Hash256::from(session.request.hash_algorithm().hash(preimage)) == payment_hash {
                    attempt.preimage = Some(preimage);
                    self.store.insert_attempt(attempt.clone());
                    session.update_with_attempt(attempt);
                    break;
                }
            }
        }
        let learned_preimage = !had_preimage && session.payment_preimage().is_some();
        if session.status != PaymentStatus::Success || (was_success && !learned_preimage) {
            return;
        }
        session.set_success_status();
        self.store.insert_payment_session(session);
        self.network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PaymentSucceeded(payment_hash),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    /// Publish failure only after dispatch has stopped and every issued effect
    /// has completed. A channel acknowledgement must never wait for this step.
    fn advance_payment_result(&self, state: &PaymentActorState) {
        let payment_hash = state.payment_hash;
        let Some(mut session) = self.store.get_payment_session(payment_hash) else {
            return;
        };
        if session.status == PaymentStatus::Success {
            return;
        }
        let persisted = self.store.get_persisted_payment_status(payment_hash);
        let outstanding = !state.pending_add_results.is_empty()
            || session.has_outstanding_channel_work(&self.store);
        let mut changed = persisted != Some(session.status);
        if persisted == Some(PaymentStatus::Failed) && outstanding {
            session.retry_stopped = true;
            session.status = PaymentStatus::Inflight;
            changed = true;
        }
        if !session.status.is_final()
            && !session.retry_stopped
            && session.attempts_count() > 0
            && session.remain_amount() > 0
            && session.retry_times() >= session.try_limit
        {
            session.retry_stopped = true;
            session.last_error = Some("Can not send payment with limited attempts".to_owned());
            changed = true;
        }
        if session.retry_stopped && !session.status.is_final() {
            if outstanding {
                if session.status != PaymentStatus::Inflight {
                    session.status = PaymentStatus::Inflight;
                    changed = true;
                }
            } else {
                let error = session
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "Payment attempts exhausted".to_owned());
                session.set_failed_status(&error);
                changed = true;
            }
        }
        if changed && !session.is_dry_run() {
            self.store.insert_payment_session(session);
        }
    }

    fn check_payment_final(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &PaymentActorState,
    ) {
        let Some(session) = self.store.get_payment_session(state.payment_hash) else {
            error!("Can't find payment session");
            myself.stop(Some("Can't find payment session".to_string()));
            return;
        };
        if session.status.is_final()
            && state.pending_add_results.is_empty()
            && !session.has_pending_payment_work(&self.store)
        {
            self.store.clear_attempts_channel_index(state.payment_hash);
            myself.stop(Some(format!(
                "Payment complete with status {:?}",
                session.status
            )));
        }
    }

    async fn update_graph_with_tlc_fail(
        &self,
        network: &ActorRef<NetworkActorMessage>,
        tlc_error_detail: &TlcErr,
    ) {
        let error_code = tlc_error_detail.error_code();
        // https://github.com/lightning/bolts/blob/master/04-onion-routing.md#rationale-6
        // we now still update the graph, maybe we need to remove it later?
        if error_code.is_update() {
            if let Some(TlcErrData::ChannelFailed {
                channel_update: Some(channel_update),
                ..
            }) = &tlc_error_detail.extra_data
            {
                network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastMessages(vec![
                            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update.clone()),
                        ]),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
        match tlc_error_detail.error_code() {
            TlcErrorCode::PermanentChannelFailure
            | TlcErrorCode::ChannelDisabled
            | TlcErrorCode::UnknownNextPeer => {
                let channel_outpoint = tlc_error_detail
                    .error_channel_outpoint()
                    .expect("expect channel outpoint");
                let mut graph = self.network_graph.write().await;
                debug!("debug mark channel failed: {:?}", channel_outpoint);
                graph.mark_channel_failed(&channel_outpoint);
            }
            TlcErrorCode::PermanentNodeFailure => {
                let node_id = tlc_error_detail.error_node_id().expect("expect node id");
                let mut graph = self.network_graph.write().await;
                graph.mark_node_failed(node_id);
            }
            _ => {}
        }
    }

    async fn resend_payment_attempt(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
    ) -> Result<(), Error> {
        assert!(attempt.is_retrying());

        if attempt.last_error.as_ref().is_some_and(|e| !e.is_empty()) {
            let amount = session.retry_amount(attempt).ok_or_else(|| {
                Error::SendPaymentError(
                    "Retry payment amount overflows remaining amount".to_string(),
                )
            })?;
            let max_fee = session.remain_fee_amount();
            let channel_stats = {
                let graph = self.network_graph.read().await;
                graph.channel_stats()
            };

            let channel_stats = GraphChannelStat::new(Some(channel_stats));
            let payment_state =
                SendPaymentState::with_channel_stats(session.request.clone(), channel_stats);
            #[cfg(not(target_arch = "wasm32"))]
            let hops = self
                .build_route_in_spawn_task(amount, None, max_fee, payment_state)
                .await
                .map_err(|e| {
                    Error::BuildPaymentRouteError(format!("Failed to build route, {}", e))
                })?;

            #[cfg(target_arch = "wasm32")]
            let hops = self
                .build_route(amount, None, max_fee, payment_state)
                .await
                .map_err(|e| {
                    Error::BuildPaymentRouteError(format!("Failed to build route, {}", e))
                })?;

            check_trampoline_outgoing_tlc_expiry(session, &hops)?;
            attempt.update_route(hops);
        }

        self.send_attempt(myself, state, session, attempt).await?;
        Ok(())
    }

    async fn build_payment_routes(
        &self,
        session: &mut PaymentSession,
    ) -> Result<Vec<Attempt>, Error> {
        let (source, channel_stats) = {
            let graph = self.network_graph.read().await;
            (graph.get_source_pubkey(), graph.channel_stats())
        };
        let active_parts = session.attempts().filter(|a| a.is_active()).count();
        let is_self_pay = source == session.request.target_pubkey;
        let mut remain_amount = session.remain_amount();
        let mut max_fee = session.remain_fee_amount();
        let mut result = vec![];

        if remain_amount == 0 {
            let error = format!("Send amount {} is not expected to be 0", remain_amount);
            return Err(Error::SendPaymentError(error));
        }

        let mut channel_stats = GraphChannelStat::new(Some(channel_stats));
        let amount_low_bound = Some(1);
        // Derive the next id from the highest retained id: a retried session can
        // keep settled attempts, so ids may not be contiguous and `attempts_count`
        // could collide with an existing settled attempt.
        let mut attempt_id = session
            .attempts()
            .map(|attempt| attempt.id)
            .max()
            .unwrap_or(0);
        let mut target_amount = remain_amount;
        let mut single_path_max = None;
        let mut iteration = 0_u64;

        if session.max_parts() > 1 && !is_self_pay {
            let path_max = {
                let graph = self.network_graph.read().await;
                graph.find_path_max_capacity(&SendPaymentState::with_channel_stats(
                    session.request.clone(),
                    channel_stats.clone(),
                ))?
            };
            if path_max
                .checked_mul(session.max_parts() as u128)
                .ok_or_else(|| {
                    Error::SendPaymentError("MPP path capacity calculation overflows".to_string())
                })?
                < remain_amount
            {
                let error = "Failed to build enough routes for MPP payment".to_string();
                return Err(Error::SendPaymentError(error));
            }
            single_path_max = Some(path_max);
        }

        let max_new_parts = session
            .max_parts()
            .checked_sub(active_parts)
            .ok_or_else(|| {
                Error::SendPaymentError("active payment parts exceed max_parts".to_string())
            })?;
        while (result.len() < max_new_parts) && remain_amount > 0 {
            iteration = iteration.checked_add(1).ok_or_else(|| {
                Error::SendPaymentError("route build iteration overflows".to_string())
            })?;

            debug!(
                "build route iteration {}, target_amount: {} amount_low_bound: {:?} remain_amount: {}, max_parts: {}, max_fee: {:?}",
                iteration,
                target_amount,
                amount_low_bound,
                remain_amount,
                session.max_parts(),
                max_fee,
            );
            let payment_state = SendPaymentState::with_channel_stats(
                session.request.clone(),
                channel_stats.clone(),
            );
            #[cfg(not(target_arch = "wasm32"))]
            let build_route_result = self
                .build_route_in_spawn_task(
                    target_amount,
                    amount_low_bound,
                    max_fee,
                    payment_state.clone(),
                )
                .await;
            #[cfg(target_arch = "wasm32")]
            let build_route_result = self
                .build_route(target_amount, amount_low_bound, max_fee, payment_state)
                .await;

            match build_route_result {
                Err(e) => {
                    error!("build_payment_routes failed to build route: {}", e);
                    let error = if session.request.trampoline_context.is_some() {
                        session
                            .last_error_code
                            .get_or_insert(TlcErrorCode::TemporaryNodeFailure);
                        TlcErrorCode::TemporaryNodeFailure.as_ref().to_string()
                    } else {
                        format!("Failed to build route, {}", e)
                    };
                    return Err(Error::SendPaymentError(error));
                }
                Ok(mut hops) => {
                    assert_ne!(hops[0].funding_tx_hash, Hash256::default());

                    // Embed trampoline onion in the last hop if available
                    // This is needed both for actual payments and dry_run to get correct fee calculations
                    if let Some(trampoline) = &session.request.trampoline_context {
                        if let Some(last_hop) = hops.last_mut() {
                            last_hop.set_trampoline_onion(
                                trampoline.remaining_trampoline_onion.clone(),
                            );
                        }
                        self.apply_trampoline_forwarding_fee(session, source, &hops, &mut max_fee)
                            .await?;
                    }

                    check_trampoline_outgoing_tlc_expiry(session, &hops)?;

                    let new_attempt_id = if session.is_dry_run() {
                        0
                    } else {
                        attempt_id = attempt_id.checked_add(1).ok_or_else(|| {
                            Error::SendPaymentError("attempt id overflows".to_string())
                        })?;
                        attempt_id
                    };

                    let attempt = session.new_attempt(
                        new_attempt_id,
                        source,
                        session.request.target_pubkey,
                        hops,
                    );

                    let session_route = &attempt.route;
                    let route_channels: Vec<_> = session_route
                        .channel_outpoints()
                        .map(|(from, channel_outpoint, amount)| {
                            (from, channel_outpoint.clone(), amount)
                        })
                        .collect();

                    {
                        let graph = self.network_graph.read().await;
                        for (from, channel_outpoint, amount) in &route_channels {
                            if let Some(sent_node) =
                                graph.get_channel_sent_node(channel_outpoint, *from)
                            {
                                channel_stats.add_channel(channel_outpoint, sent_node, *amount);
                            }
                        }
                    }
                    let current_amount = session_route.receiver_amount();
                    if session.request.use_trampoline_routing() {
                        remain_amount =
                            remain_amount.checked_sub(target_amount).ok_or_else(|| {
                                Error::SendPaymentError(
                                    "Trampoline route amount exceeds remaining amount".to_string(),
                                )
                            })?;
                    } else {
                        remain_amount =
                            remain_amount.checked_sub(current_amount).ok_or_else(|| {
                                Error::SendPaymentError(
                                    "Route amount exceeds remaining amount".to_string(),
                                )
                            })?;
                    }
                    target_amount = if let Some(single) = single_path_max {
                        single.min(remain_amount)
                    } else {
                        remain_amount
                    };
                    if let Some(fee) = max_fee {
                        max_fee = Some(fee.checked_sub(session_route.fee()).ok_or_else(|| {
                            Error::SendPaymentError(
                                "max_fee_amount is too low for selected route".to_string(),
                            )
                        })?);
                    }
                    result.push(attempt);
                    let remaining_slots = session
                        .max_parts()
                        .checked_sub(result.len())
                        .ok_or_else(|| {
                            Error::SendPaymentError(
                                "built payment parts exceed max_parts".to_string(),
                            )
                        })?;
                    let max_remaining_amount = current_amount
                        .checked_mul(remaining_slots as u128)
                        .ok_or_else(|| {
                            Error::SendPaymentError(
                                "MPP remaining amount calculation overflows".to_string(),
                            )
                        })?;
                    if remain_amount > 0 && remain_amount > max_remaining_amount {
                        break;
                    }
                }
            };
        }

        if remain_amount > 0 {
            let error = "Failed to build enough routes for MPP payment".to_string();
            return Err(Error::SendPaymentError(error));
        }

        for attempt in &result {
            session.append_attempt(attempt.clone());
        }

        return Ok(result);
    }

    async fn apply_trampoline_forwarding_fee(
        &self,
        session: &mut PaymentSession,
        source: Pubkey,
        hops: &[PaymentHopData],
        max_fee: &mut Option<u128>,
    ) -> Result<(), Error> {
        if session.request.trampoline_context.is_none() {
            return Ok(());
        }

        let first_hop = hops.first().ok_or_else(|| {
            Error::SendPaymentError("Trampoline forwarding requires at least one hop".to_string())
        })?;
        let channel_outpoint = OutPoint::new(first_hop.funding_tx_hash.into(), 0);
        let fee_rate = {
            let graph = self.network_graph.read().await;
            let (Some(_info), Some(update)) =
                graph.get_outbound_channel_info_and_update(&channel_outpoint, source)
            else {
                return Err(Error::SendPaymentError(
                    "get outbound info failed".to_string(),
                ));
            };
            update.fee_rate
        };

        let local_fee =
            calculate_tlc_forward_fee(first_hop.amount, fee_rate as u128).map_err(|err| {
                Error::SendPaymentError(format!(
                    "Trampoline forwarding fee calculation failed: {:?}",
                    err
                ))
            })?;

        let cur_max_fee = max_fee.ok_or_else(|| {
            Error::SendPaymentError("Trampoline forwarding requires max_fee_amount".to_string())
        })?;
        if cur_max_fee < local_fee {
            session.last_error_code = Some(TlcErrorCode::FeeInsufficient);
            error!(
                "not enough fee budget for trampoline forwarding: budget {}, required {}",
                cur_max_fee, local_fee
            );
            return Err(Error::SendPaymentError(
                TlcErrorCode::FeeInsufficient.as_ref().to_string(),
            ));
        }
        *max_fee = Some(cur_max_fee.checked_sub(local_fee).ok_or_else(|| {
            Error::SendPaymentError("Trampoline forwarding fee insufficient".to_string())
        })?);
        Ok(())
    }

    async fn send_payment_onion_packet(
        &self,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
    ) -> Result<(), Error> {
        check_trampoline_outgoing_tlc_expiry(session, &attempt.route_hops)?;

        assert_ne!(attempt.route_hops[0].funding_tx_hash, Hash256::default());

        let peeled_onion_packet = match PeeledPaymentOnionPacket::create_with_session_key_fn(
            || Privkey::from_slice(KeyPair::generate_random_key().as_ref()),
            attempt.route_hops.clone(),
            Some(attempt.hash.as_ref().to_vec()),
            &Secp256k1::signing_only(),
        ) {
            Ok((packet, session_key)) => {
                attempt.session_key.copy_from_slice(session_key.as_ref());
                packet
            }
            Err(e) => {
                let err = format!(
                    "Failed to create onion packet: {:?}, error: {:?}",
                    attempt.hash, e
                );
                self.set_attempt_fail_with_error(session, attempt, None, &err, false);
                return Err(Error::FirstHopError(err, false));
            }
        };

        if !session.is_dry_run() {
            self.store.insert_attempt(attempt.clone());
        }

        match call_t!(
            self.network,
            |tx| {
                NetworkActorMessage::new_command(NetworkActorCommand::SendPaymentOnionPacket(
                    SendOnionPacketCommand {
                        peeled_onion_packet,
                        previous_tlc: None,
                        payment_hash: attempt.payment_hash,
                        attempt_id: Some(attempt.id),
                    },
                    tx,
                ))
            },
            DEFAULT_CHAIN_ACTOR_TIMEOUT
        )
        .expect(ASSUME_NETWORK_ACTOR_ALIVE)
        {
            Err(error_detail) => {
                self.update_graph_with_tlc_fail(&self.network, &error_detail)
                    .await;
                let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                    attempt,
                    error_detail.clone(),
                    true,
                );
                let err = format!(
                    "Failed to send onion packet with error {}",
                    error_detail.error_code_as_str()
                );
                self.set_attempt_fail_with_error(
                    session,
                    attempt,
                    Some(error_detail.error_code),
                    &err,
                    need_to_retry,
                );
                return Err(Error::FirstHopError(err, need_to_retry));
            }
            Ok(_) => {
                return Ok(());
            }
        }
    }

    fn set_payment_fail_with_error(
        &self,
        session: &mut PaymentSession,
        error: &str,
        error_code: Option<TlcErrorCode>,
    ) {
        if session.status == PaymentStatus::Success {
            return;
        }
        // Persist before acknowledging or retiring any source TLC. Keeping the
        // public status non-final prevents CCH from cancelling its incoming leg.
        session.retry_stopped = true;
        session.last_error = Some(error.to_owned());
        session.last_updated_at = now_timestamp_as_millis_u64();
        if !session.status.is_final() {
            session.status = PaymentStatus::Inflight;
        }
        if error_code.is_some() {
            session.last_error_code = error_code;
        }
        if !session.is_dry_run() {
            self.store.insert_payment_session(session.clone());
        }
    }

    fn set_attempt_fail_with_error(
        &self,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
        error_code: Option<TlcErrorCode>,
        error: &str,
        retryable: bool,
    ) {
        if !retryable && !session.active_attempts().iter().any(|a| a.id != attempt.id) {
            self.set_payment_fail_with_error(session, error, error_code);
        }

        attempt.set_failed_status(
            error,
            retryable && !session.retry_stopped && session.status != PaymentStatus::Success,
        );
        if !session.is_dry_run() {
            self.store.insert_attempt(attempt.clone());
        }
    }

    fn channel_owns_attempt(&self, attempt: &Attempt) -> bool {
        let Some(channel_outpoint) = attempt.first_hop_channel_outpoint() else {
            return false;
        };
        self.store
            .get_channel_state_by_outpoint(channel_outpoint)
            .is_some_and(|channel_state| {
                let owns_update = if attempt.is_retrying()
                    && matches!(channel_state.state, fiber_types::ChannelState::Closed(_))
                {
                    // Closed queued Adds only deliver failure; they can no longer execute this retry.
                    channel_state.tlc_state.all_tlcs().any(|tlc| {
                        !tlc.applied_flags
                            .contains(fiber_types::AppliedFlags::REMOVE)
                            && tlc.payment_hash == attempt.payment_hash
                            && tlc.attempt_id == Some(attempt.id)
                    })
                } else {
                    channel_state.owns_payment_attempt(attempt.payment_hash, attempt.id)
                };
                owns_update
                    || channel_state.session_v2.iter().any(|session| {
                        session.remove_effects.iter().any(|(tlc, execution)| {
                            tlc.payment_hash == attempt.payment_hash
                                && tlc.attempt_id == Some(attempt.id)
                                && (attempt.status == AttemptStatus::Created
                                    || execution.is_none_or(|number| number == attempt.tried_times))
                        })
                    })
            })
    }

    async fn send_attempt(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
    ) -> Result<(), Error> {
        if let Err(err) = self.send_payment_onion_packet(session, attempt).await {
            let need_retry = matches!(err, Error::FirstHopError(_, true));
            if need_retry {
                debug!("Retrying payment attempt due to first hop error: {:?}", err);
                self.register_payment_retry(myself, state, Some(attempt.id));
                return Ok(());
            } else {
                self.set_attempt_fail_with_error(session, attempt, None, &err.to_string(), false);
                return Err(err);
            }
        }
        state.pending_add_results.insert(attempt.id);
        Ok(())
    }

    /// Resume the payment session
    async fn resume_payment_session(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
    ) -> Result<(), Error> {
        let payment_hash = state.payment_hash;

        self.update_graph().await;
        let Some(mut session) = self.store.get_payment_session(payment_hash) else {
            return Err(Error::InvalidParameter(payment_hash.to_string()));
        };

        if session.status.is_final() || session.retry_stopped {
            return Ok(());
        }

        self.retry_payment_attempt(myself.clone(), state, &mut session, attempt_id)
            .await?;

        if !self.payment_need_more_retry(&mut session)? {
            return Ok(());
        }

        // here we begin to create attempts and routes for the payment session,
        // it depends on the path finding algorithm to create how many of attempts,
        // if a payment can not be met in the network graph, an build path error will be returned
        // and no attempts be stored in the payment session and db.
        let mut attempts = self
            .build_payment_routes(&mut session)
            .await
            .inspect_err(|e| {
                self.set_payment_fail_with_error(&mut session, &e.to_string(), None);
            })?;

        for attempt in attempts.iter_mut() {
            self.send_attempt(myself.clone(), state, &mut session, attempt)
                .await?;
        }

        if let Ok(true) = self.payment_need_more_retry(&mut session) {
            self.register_payment_retry(myself, state, None);
        }

        Ok(())
    }

    async fn retry_payment_attempt(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        session: &mut PaymentSession,
        attempt_id: Option<u64>,
    ) -> Result<(), Error> {
        if session.retry_stopped || session.status.is_final() {
            return Ok(());
        }
        let Some(attempt_id) = attempt_id else {
            return Ok(());
        };
        if state.pending_add_results.contains(&attempt_id) {
            return Ok(());
        }
        state.attempts_with_retry_work.insert(attempt_id);

        match self.store.get_attempt(session.payment_hash(), attempt_id) {
            Some(mut attempt) if attempt.is_retrying() => {
                if self.channel_owns_attempt(&attempt) {
                    return Ok(());
                }
                match self
                    .resend_payment_attempt(myself, state, session, &mut attempt)
                    .await
                {
                    Err(err) if session.allow_mpp() => {
                        // usually `resend_payment_route` will only try build a route with same amount,
                        // because most of the time, resend payment caused by the first hop
                        // error with WaitingTlcAck, if resend failed we should try more attempts in MPP,
                        // so we may create more attempts with different split amounts
                        attempt.set_failed_status(&err.to_string(), false);
                        self.store.insert_attempt(attempt);
                    }
                    Err(err) => {
                        self.set_attempt_fail_with_error(
                            session,
                            &mut attempt,
                            None,
                            &err.to_string(),
                            false,
                        );
                        return Err(err);
                    }
                    _ => {}
                }
            }
            Some(mut attempt) if attempt.status == AttemptStatus::Created => {
                if self.channel_owns_attempt(&attempt) {
                    debug!(
                        payment_hash = ?session.payment_hash(),
                        attempt_id = attempt.id,
                        "Skipping channel-owned Created payment attempt"
                    );
                } else {
                    warn!(
                        payment_hash = ?session.payment_hash(),
                        attempt_id = attempt.id,
                        "Retrying orphan Created payment attempt"
                    );
                    self.send_attempt(myself, state, session, &mut attempt)
                        .await?;
                }
            }
            Some(_) => {
                // no retry for non-retryable attempts
            }
            None => {
                return Err(Error::InvalidParameter(format!(
                    "Attempt with id {:?} not found for payment hash: {:?}",
                    attempt_id,
                    session.payment_hash()
                )));
            }
        }

        Ok(())
    }

    fn get_payment_session_with_attempt(
        &self,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
    ) -> (Option<PaymentSession>, Option<Attempt>) {
        let payment_session = self.store.get_payment_session(payment_hash);
        let attempt =
            attempt_id.and_then(|attempt_id| self.store.get_attempt(payment_hash, attempt_id));

        (payment_session, attempt)
    }

    async fn complete_closed_local_add(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        channel_id: Hash256,
        channel_outpoint: OutPoint,
        command: &fiber_types::AddTlcCommand,
    ) -> Result<(), String> {
        let attempt_id = command.attempt_id.ok_or("Closed Add has no attempt ID")?;
        if command.payment_hash != state.payment_hash {
            return Err("Closed Add payment identity mismatch".to_owned());
        }
        let (Some(session), Some(attempt)) =
            self.get_payment_session_with_attempt(state.payment_hash, Some(attempt_id))
        else {
            return Err("Closed Add payment or attempt not found".to_owned());
        };
        if !attempt.first_hop_channel_outpoint_eq(&channel_outpoint) {
            return Ok(());
        }
        if (session.status == PaymentStatus::Success || !session.status.is_final())
            && matches!(
                attempt.status,
                AttemptStatus::Created | AttemptStatus::Retrying
            )
        {
            let packet = command
                .onion_packet
                .clone()
                .ok_or("Closed Add has no execution packet")?
                .into_sphinx_onion_packet()
                .map_err(|e| e.to_string())?;
            let key = secp256k1::SecretKey::from_byte_array(&attempt.session_key)
                .map_err(|e| e.to_string())?;
            // A retry gets a new onion key, so an old queued command cannot fail the new execution.
            if packet.public_key != key.public_key(&Secp256k1::signing_only()) {
                return Ok(());
            }
            if state.processed_closed_adds.insert((channel_id, attempt_id)) {
                let local_node = self.network_graph.read().await.get_source_pubkey();
                let failure = TlcErr::new_channel_fail(
                    TlcErrorCode::PermanentChannelFailure,
                    local_node,
                    channel_outpoint,
                    None,
                );
                if attempt.status == AttemptStatus::Created
                    || session.status == PaymentStatus::Success
                {
                    self.handle_add_tlc_result_event(
                        myself,
                        state,
                        Some(attempt_id),
                        Err((
                            ProcessingChannelError::InvalidState(
                                "Channel closed before AddTlc execution".to_owned(),
                            ),
                            failure,
                        )),
                    )
                    .await;
                } else {
                    // Retrying is already a durable failed outcome; re-arm it without counting failure twice.
                    self.update_graph_with_tlc_fail(&self.network, &failure)
                        .await;
                    self.register_payment_retry(myself, state, Some(attempt_id));
                }
            }
        }
        let session = self
            .store
            .get_payment_session(state.payment_hash)
            .ok_or("Closed Add payment disappeared")?;
        let current = self
            .store
            .get_attempt(state.payment_hash, attempt_id)
            .ok_or("Closed Add attempt disappeared")?;
        if current.status == AttemptStatus::Created && !session.status.is_final() {
            return Err("Closed Add outcome is not persisted".to_owned());
        }
        if !session.is_dry_run() {
            self.store.insert_payment_session(session.clone());
            if session.status.is_final() {
                self.store.clear_attempts_channel_index(state.payment_hash);
            }
        }
        Ok(())
    }

    async fn handle_add_tlc_result_event(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
    ) {
        if !matches!(
            add_tlc_result,
            Err((ProcessingChannelError::WaitingTlcAck, _))
        ) {
            if let Some(id) = attempt_id {
                state.pending_add_results.remove(&id);
            }
        }
        let payment_hash = state.payment_hash;
        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, attempt_id)
        else {
            warn!(
                "Payment session not found: {:?} attempt_id: {:?}",
                payment_hash, attempt_id
            );
            return;
        };

        if attempt.status.is_final() {
            return;
        }
        match add_tlc_result {
            Ok(_) => {
                attempt.set_inflight_status();
                self.network_graph
                    .write()
                    .await
                    .track_attempt_router(&attempt);
                self.store.insert_attempt(attempt);
            }
            Err((ProcessingChannelError::WaitingTlcAck, _)) => {
                // do nothing
            }
            Err((error, tlc_err)) => {
                self.update_graph_with_tlc_fail(&self.network, &tlc_err)
                    .await;
                let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                    &attempt,
                    tlc_err.clone(),
                    true,
                );
                self.set_attempt_fail_with_error(
                    &mut session,
                    &mut attempt,
                    Some(tlc_err.error_code),
                    &error.to_string(),
                    need_to_retry,
                );

                if attempt.is_retrying() {
                    self.register_payment_retry(myself.clone(), state, Some(attempt.id));
                }
            }
        }
    }

    pub(super) async fn handle_remove_tlc_event(
        &self,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
        local_error: Option<&TlcErr>,
        completion: Option<&PaymentTlcRemoveContext>,
    ) -> Result<bool, String> {
        let payment_hash = state.payment_hash;
        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, attempt_id)
        else {
            error!(
                "Payment session or attempt not found for payment hash: {:?}, attempt id: {:?}",
                payment_hash, attempt_id
            );
            return Err("Payment session or attempt missing for TLC removal".to_string());
        };

        let mut already_processed = false;
        if let Some(completion) = completion {
            if !attempt.first_hop_channel_outpoint_eq(&completion.channel_outpoint) {
                return Ok(false);
            }
            if attempt.is_success() {
                // The attempt and aggregate session are separate writes. Recovery
                // can repeat delivery after the attempt write but before the session
                // write; repair the aggregate before acknowledging the source outbox.
                session.update_with_attempt(attempt);
                if !session.is_dry_run() {
                    self.store.insert_payment_session(session.clone());
                    if session.status.is_final() {
                        self.store.clear_attempts_channel_index(payment_hash);
                    }
                }
                return Ok(false);
            }
            let same_execution = attempt.tried_times == completion.tried_times;
            let just_retried = attempt.is_retrying()
                && attempt.tried_times == completion.tried_times.saturating_add(1);
            if !same_execution && !just_retried {
                return Ok(false);
            }
            // Duplicates must not increment retry counts, release route usage twice, or
            // fail the next execution of this attempt after its route has been rebuilt.
            if attempt.is_failed() || attempt.is_retrying() {
                already_processed = true;
            }
        }

        if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = &reason {
            if Hash256::from(
                session
                    .request
                    .hash_algorithm()
                    .hash(fulfill.payment_preimage),
            ) != payment_hash
            {
                return Err("Invalid payment fulfillment preimage".to_owned());
            }
            already_processed = attempt.is_success();
        }
        let mut retry = already_processed && attempt.is_retrying();
        if !already_processed {
            match reason {
                RemoveTlcReason::RemoveTlcFulfill(fulfill) => {
                    self.network_graph
                        .write()
                        .await
                        .record_attempt_success(&attempt);
                    attempt.set_success_status();
                    attempt.preimage = Some(fulfill.payment_preimage);
                    self.store.insert_attempt(attempt.clone());

                    session.update_with_attempt(attempt);
                    if !session.is_dry_run() {
                        self.store.insert_payment_session(session.clone());
                        // Clean up channel index to prevent retries on channel ready
                        if session.status.is_final() {
                            self.store
                                .clear_attempts_channel_index(session.payment_hash());
                        }
                    }
                }
                RemoveTlcReason::RemoveTlcFail(reason) => {
                    let (tlc_error, route_index) = if let Some(local_error) = local_error {
                        (local_error.clone(), None)
                    } else {
                        match reason.decode(&attempt.session_key, attempt.hops_public_keys()) {
                            Some(decoded)
                                if decoded.hop_index.saturating_add(1)
                                    < attempt.route.nodes.len() =>
                            {
                                (decoded.error, Some(decoded.hop_index + 1))
                            }
                            _ => {
                                debug_event!(self.network, "InvalidOnionError");
                                (TlcErr::new(TlcErrorCode::InvalidOnionError), None)
                            }
                        }
                    };
                    let need_to_retry = if let Some(route_index) = route_index {
                        self.network_graph.write().await.record_attempt_fail_at_hop(
                            &attempt,
                            tlc_error.clone(),
                            route_index,
                        )
                    } else {
                        self.network_graph.write().await.record_attempt_fail(
                            &attempt,
                            tlc_error.clone(),
                            false,
                        )
                    };
                    debug!(
                        "payment_hash: {:?} set attempt failed with: {:?} need_to_retry: {:?}",
                        payment_hash,
                        tlc_error.error_code.as_ref(),
                        need_to_retry
                    );
                    state.last_error_packet = Some(reason.clone());

                    self.set_attempt_fail_with_error(
                        &mut session,
                        &mut attempt,
                        Some(tlc_error.error_code),
                        &tlc_error.to_string(),
                        need_to_retry,
                    );

                    retry = attempt.is_retrying();
                }
            }
        }

        #[cfg(debug_assertions)]
        {
            if let Some(payment_session) = self.store.get_payment_session(payment_hash) {
                debug_event!(
                    self.network,
                    format!(
                        "after on_remove_tlc_event session_status: {:?}",
                        payment_session.status
                    )
                );
            }
        }
        if let Some(completion) = completion {
            // Persist stop intent before acknowledging an exhausted or replayed failure.
            // Reuse the existing session API without applying another attempt failure.
            if let Some(attempt) =
                attempt_id.and_then(|id| self.store.get_attempt(payment_hash, id))
            {
                if attempt.is_failed()
                    && !session.status.is_final()
                    && !session.active_attempts().iter().any(|a| a.id != attempt.id)
                {
                    self.set_payment_fail_with_error(
                        &mut session,
                        attempt.last_error.as_deref().unwrap_or_default(),
                        local_error.map(|error| error.error_code),
                    );
                }
            }
            // Record only after persistence; duplicates still receive the completion receipt.
            retry &= state
                .processed_onchain_removals
                .insert((completion.channel_id, completion.tlc_id));
        }
        Ok(retry && !session.retry_stopped && session.status != PaymentStatus::Success)
    }

    /// Apply an on-chain-confirmed fulfill to one exact payment attempt.
    ///
    /// This is intentionally keyed by both the payment hash owned by this actor and the local
    /// attempt id persisted on the first-hop TLC. Replaying the same proof is a no-op once both the
    /// attempt and aggregate payment session have converged.
    async fn handle_reconcile_onchain_fulfill(
        &self,
        state: &PaymentActorState,
        attempt_id: u64,
        payment_preimage: Hash256,
    ) -> Result<(), Error> {
        let payment_hash = state.payment_hash;
        let persisted_session_status = self.store.get_persisted_payment_status(payment_hash);
        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, Some(attempt_id))
        else {
            return Err(Error::InvalidParameter(format!(
                "Payment session or attempt not found for on-chain fulfill: payment_hash={payment_hash:?}, attempt_id={attempt_id}"
            )));
        };

        if let Some(existing_preimage) = attempt.preimage {
            if existing_preimage != payment_preimage {
                return Err(Error::InvalidParameter(format!(
                    "On-chain fulfill preimage conflicts with attempt: payment_hash={payment_hash:?}, attempt_id={attempt_id}"
                )));
            }
        }

        if Hash256::from(session.request.hash_algorithm().hash(payment_preimage)) != payment_hash {
            return Err(Error::InvalidParameter(
                "Invalid on-chain payment preimage".to_owned(),
            ));
        }
        let attempt_changed = !attempt.is_success() || attempt.preimage.is_none();
        if attempt_changed {
            self.network_graph
                .write()
                .await
                .record_attempt_success(&attempt);
            attempt.set_success_status();
            attempt.preimage = Some(payment_preimage);
            self.store.insert_attempt(attempt.clone());
        }

        let previous_session_status = session.status;
        session.update_with_attempt(attempt);

        session.set_success_status();

        let session_record_changed = persisted_session_status != Some(session.status);
        if !session.is_dry_run()
            && (attempt_changed
                || session.status != previous_session_status
                || session_record_changed)
        {
            self.store.insert_payment_session(session.clone());
            if session.status.is_final() {
                self.store
                    .clear_attempts_channel_index(session.payment_hash());
            }
        }

        Ok(())
    }

    fn payment_need_more_retry(&self, session: &mut PaymentSession) -> Result<bool, Error> {
        session.flush_attempts(&self.store);
        if session.status == PaymentStatus::Success || session.retry_stopped {
            return Ok(false);
        }
        if session.remain_amount() > 0 && session.retry_times() >= session.try_limit {
            let err = "Can not send payment with limited attempts";
            self.set_payment_fail_with_error(session, err, None);
            return Err(Error::SendPaymentError(err.to_string()));
        }
        // No available part slot or no unallocated amount is only a wait.
        Ok(session.allow_more_attempts())
    }

    fn register_payment_retry(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
    ) {
        // This is a performance tuning result, the basic idea is when there are more pending
        // retrying payment in ractor framework, we will increase the delay time to avoid
        // flooding the network actor with too many retrying payments.
        let Some(next_retry_count) = state.retry_send_payment_count.checked_add(1) else {
            error!("retry_send_payment_count overflow");
            return;
        };
        state.retry_send_payment_count = next_retry_count;
        let Ok(retry_count) = u64::try_from(state.retry_send_payment_count) else {
            error!("retry_send_payment_count does not fit into u64");
            return;
        };
        let Some(delay) = retry_count.checked_mul(20_u64) else {
            error!("retry payment delay overflow");
            return;
        };
        if let Some(id) = attempt_id {
            state.attempts_with_retry_work.insert(id);
        }
        myself.send_after(Duration::from_millis(delay), move || {
            PaymentActorMessage::RetrySendPayment(attempt_id)
        });
    }

    async fn on_send_payment(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        payment_data: SendPaymentData,
    ) -> Result<SendPaymentResponse, Error> {
        #[cfg(all(feature = "metrics", not(target_arch = "wasm32")))]
        let payment_hash = payment_data.payment_hash;
        #[cfg(all(feature = "metrics", not(target_arch = "wasm32")))]
        let start_time = std::time::Instant::now();
        let res = self
            .send_payment_with_payment_data(myself, state, payment_data)
            .await;

        #[cfg(all(feature = "metrics", not(target_arch = "wasm32")))]
        {
            if let Some(count) = self
                .network_graph
                .read()
                .await
                .payment_find_path_stats
                .lock()
                .get(&payment_hash)
            {
                metrics::gauge!(crate::metrics::SEND_PAYMENT_FIND_PATH_COUNT).set(*count as u32);
            }
            let duration = start_time.elapsed().as_millis();
            metrics::histogram!("fiber.send_payment_cost_time").record(duration as u32);
        }
        res
    }

    async fn send_payment_with_payment_data(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        payment_data: SendPaymentData,
    ) -> Result<SendPaymentResponse, Error> {
        // initialize the payment session in db and begin the payment process lifecycle
        if let Some(payment_session) = self.store.get_payment_session(payment_data.payment_hash) {
            // we only allow retrying payment session with status failed
            if payment_session.status != PaymentStatus::Failed {
                return Err(Error::InvalidParameter(format!(
                    "Payment session already exists: {} with payment session status: {:?}",
                    payment_data.payment_hash, payment_session.status
                )));
            } else {
                // even if the payment session is failed, we still need to check whether
                // some attempts are still flight state, this means some middle hops
                // haven't send back the result of the onion packet, so we can not retry the payment session
                // otherwise, we are sure it's safe to cleanup all the previous attempts
                if payment_session
                    .attempts()
                    .any(|attempt| attempt.is_inflight() || self.channel_owns_attempt(attempt))
                {
                    return Err(Error::InvalidParameter(format!(
                        "Payment session {} has attempts that are in flight state, can not retry",
                        payment_data.payment_hash
                    )));
                }
                if !payment_data.dry_run {
                    // Keep successfully settled attempts: MPP payments can settle some
                    // parts and fail others. Deleting all attempts would make the
                    // rebuilt session forget the delivered amount and re-dispatch the
                    // full payment amount on retry, allowing overpayment.
                    self.store
                        .delete_unsettled_attempts(payment_data.payment_hash);
                }
            }
        }

        // for dry run, we only build the route and return the hops info,
        // will not store the payment session and send the onion packet
        if payment_data.dry_run {
            let mut payment_session = PaymentSession::new_session(&self.store, payment_data, 0);
            self.build_payment_routes(&mut payment_session).await?;
            return Ok(payment_session.into());
        }

        let try_limit = if payment_data.allow_mpp() {
            (payment_data.max_parts() as u32)
                .checked_mul(DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT)
                .ok_or_else(|| {
                    Error::InvalidParameter("MPP payment try limit overflows".to_string())
                })?
        } else {
            DEFAULT_PAYMENT_TRY_LIMIT
        };
        let mut payment_session = PaymentSession::new_session(&self.store, payment_data, try_limit);
        // Unsettled attempts are removed above; any attempt that survives is a
        // settled MPP part that the rebuilt session must keep accounting for.
        assert!(payment_session
            .attempts()
            .all(|attempt| attempt.is_success()));
        self.store.insert_payment_session(payment_session.clone());

        let dispatch = self.resume_payment_session(myself, state, None).await;
        self.advance_payment_result(state);
        payment_session.flush_attempts(&self.store);
        if let Err(error) = dispatch {
            if payment_session.status != PaymentStatus::Success
                && state.pending_add_results.is_empty()
                && !payment_session.has_outstanding_channel_work(&self.store)
            {
                return Err(error);
            }
        }
        return Ok(payment_session.into());
    }
}

#[cfg(test)]
mod closed_add_tests {
    use fiber_types::AttemptStatus;
    use ractor::{Actor, ActorProcessingErr, ActorRef};

    use crate::fiber::{
        channel::ChannelActorStateStore, graph::NetworkGraphStateStore,
        network::TestFiberMessageKind,
    };
    use crate::tests::test_utils::create_nodes_with_established_channel;

    use super::{
        PaymentActor, PaymentActorArguments, PaymentActorMessage, PaymentActorState,
        PaymentSessionExt,
    };

    struct RetryMailbox;

    #[async_trait::async_trait]
    impl Actor for RetryMailbox {
        type Msg = PaymentActorMessage;
        type State = ();
        type Arguments = ();
        async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
            Ok(())
        }
        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            _: Self::Msg,
            _: &mut (),
        ) -> Result<(), ActorProcessingErr> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_retry_exhaustion_does_not_publish_failed_while_tlc_is_inflight() {
        assert_retry_decision_with_inflight_tlc(true, false).await;
    }

    #[tokio::test]
    async fn test_full_part_capacity_waits_without_stopping_payment() {
        assert_retry_decision_with_inflight_tlc(false, false).await;
    }

    #[tokio::test]
    async fn test_reserved_retry_at_budget_limit_is_not_stopped_before_dispatch() {
        assert_retry_decision_with_inflight_tlc(false, true).await;
    }

    #[tokio::test]
    async fn test_stopped_payment_waits_for_add_result_before_channel_persistence() {
        use crate::fiber::channel::ProcessingChannelError;
        use crate::fiber::PaymentStatus;
        use fiber_types::{TlcErr, TlcErrorCode};

        let (a, b, _) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let hash = a
            .send_payment_keysend(&b, 1000, false)
            .await
            .unwrap()
            .payment_hash;
        a.wait_until_payment_finished(hash).await;
        a.wait_until_no_inflight_payments().await;
        let mut attempt = a.store.get_attempts(hash).pop().unwrap();
        attempt.status = AttemptStatus::Created;
        attempt.preimage = None;
        a.store.insert_attempt(attempt.clone());
        let mut session = a.store.get_payment_session(hash).unwrap();
        session.status = PaymentStatus::Inflight;
        session.retry_stopped = true;
        a.store.insert_payment_session(session);

        let actor = PaymentActor::new(
            a.store.clone(),
            a.network_graph.clone(),
            a.network_actor.clone(),
        );
        let mut state = PaymentActorState::new(PaymentActorArguments {
            payment_hash: hash,
            init_command: PaymentActorMessage::TlcUpdated,
        });
        state.pending_add_results.insert(attempt.id);
        let (mailbox, mut task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
        let mut dispatching = a.store.get_payment_session(hash).unwrap();
        dispatching.retry_stopped = false;
        actor
            .retry_payment_attempt(
                mailbox.clone(),
                &mut state,
                &mut dispatching,
                Some(attempt.id),
            )
            .await
            .unwrap();
        assert_eq!(
            a.store.get_attempt(hash, attempt.id).unwrap().session_key,
            attempt.session_key,
            "an Add already admitted to the channel mailbox must not be sent again"
        );
        assert!(!a
            .store
            .get_payment_session(hash)
            .unwrap()
            .has_outstanding_channel_work(&a.store));
        actor.advance_payment_result(&state);
        assert_eq!(
            a.store.get_persisted_payment_status(hash),
            Some(PaymentStatus::Inflight)
        );
        actor.handle_check_payment_status(mailbox.clone(), &mut state);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut task)
                .await
                .is_err(),
            "a periodic check must retain ownership of a pending channel admission"
        );

        let failure = TlcErr::new(TlcErrorCode::TemporaryChannelFailure);
        actor
            .handle_command(
                mailbox,
                &mut state,
                PaymentActorMessage::OnAddTlcResultEvent {
                    attempt_id: Some(attempt.id),
                    add_tlc_result: Err((
                        ProcessingChannelError::TlcForwardingError(failure.clone()),
                        failure,
                    )),
                },
            )
            .await
            .unwrap();
        assert!(state.pending_add_results.is_empty());
        assert_eq!(
            a.store.get_persisted_payment_status(hash),
            Some(PaymentStatus::Failed)
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    async fn assert_retry_decision_with_inflight_tlc(exhausted: bool, reserved_retry: bool) {
        use super::SendPaymentCommand;
        use crate::{fiber::PaymentStatus, gen_rand_sha256_hash};
        let (a, b, _) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let invoice = b.build_basic_invoice(1000, gen_rand_sha256_hash());
        let hash = *invoice.payment_hash();
        b.insert_invoice(invoice.clone(), None);
        a.send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
        a.wait_until_inflight(hash).await;
        let mut session = a.store.get_payment_session(hash).unwrap();
        session.request.amount = if reserved_retry { 1000 } else { 2000 };
        session.request.max_parts = Some(1);
        session.try_limit = if exhausted { 0 } else { 100 };
        if reserved_retry {
            let mut attempt = a.store.get_attempts(hash).pop().unwrap();
            attempt.status = AttemptStatus::Retrying;
            attempt.tried_times = 2;
            session.try_limit = 2;
            a.store.insert_attempt(attempt);
        }
        a.store.insert_payment_session(session.clone());
        let actor = PaymentActor::new(
            a.store.clone(),
            a.network_graph.clone(),
            a.network_actor.clone(),
        );
        if reserved_retry {
            actor.advance_payment_result(&PaymentActorState::new(PaymentActorArguments {
                payment_hash: hash,
                init_command: PaymentActorMessage::TlcUpdated,
            }));
        }
        let decision = actor.payment_need_more_retry(&mut session);
        if reserved_retry {
            let recovered = a.store.get_payment_session(hash).unwrap();
            assert!(
                !recovered.retry_stopped,
                "tried_times includes the reserved next send, not just sends already executed"
            );
            assert!(!recovered.status.is_final());
            assert!(matches!(decision, Ok(false)));
            return;
        }
        assert_eq!(
            a.store.get_persisted_payment_status(hash),
            Some(PaymentStatus::Inflight),
            "a retry decision must not publish final failure while an issued TLC can still fulfill"
        );
        if !exhausted {
            assert!(
                matches!(decision, Ok(false)),
                "full part capacity is a wait, not a terminal error"
            );
        }
    }

    #[tokio::test]
    async fn test_success_exits_without_failing_unissued_attempts_or_rewriting_session() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let (a, b, _) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let hash = a
            .send_payment_keysend(&b, 1000, false)
            .await
            .unwrap()
            .payment_hash;
        a.wait_until_payment_finished(hash).await;
        a.wait_until_no_inflight_payments().await;
        let settled = a.store.get_attempts(hash).pop().unwrap();
        let outpoint = settled.first_hop_channel_outpoint().unwrap().clone();
        // Recovery can leave planned or retryable attempts that never reached a channel.
        for (id, status) in [(2, AttemptStatus::Created), (3, AttemptStatus::Retrying)] {
            let mut unissued = settled.clone();
            unissued.id = id;
            unissued.status = status;
            unissued.preimage = None;
            a.store.insert_attempt(unissued);
        }
        let session = a.store.get_payment_session(hash).unwrap();
        assert!(!session.has_pending_payment_work(&a.store));
        assert_eq!(
            a.store
                .get_pending_attempts_by_channel_outpoint(&outpoint)
                .len(),
            2
        );

        let writes = Arc::new(AtomicUsize::new(0));
        let mut store = a.store.clone();
        let observed = writes.clone();
        store.set_watcher(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let actor = PaymentActor::new(store, a.network_graph.clone(), a.network_actor.clone());
        let (mailbox, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
        let mut state = PaymentActorState::new(PaymentActorArguments {
            payment_hash: hash,
            init_command: PaymentActorMessage::TlcUpdated,
        });
        actor
            .handle_command(mailbox, &mut state, PaymentActorMessage::TlcUpdated)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            a.store.get_attempt(hash, 2).unwrap().status,
            AttemptStatus::Created
        );
        assert_eq!(
            a.store.get_attempt(hash, 3).unwrap().status,
            AttemptStatus::Retrying
        );
        assert!(a
            .store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .is_empty());
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "an unchanged success must not publish duplicate store updates"
        );
    }

    #[tokio::test]
    async fn test_retrying_add_respects_live_owner_but_not_closed_failure_queue() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        a.hold_next_fiber_messages(b.pubkey, id, TestFiberMessageKind::CommitmentSigned, 1)
            .await;
        let hash = a
            .send_payment_keysend(&b, 1000, false)
            .await
            .unwrap()
            .payment_hash;
        a.wait_for_held_fiber_messages(1).await;
        let mut attempt = a.store.get_attempts(hash).pop().unwrap();
        b.stop().await;
        a.stop().await;
        let actor = PaymentActor::new(
            a.store.clone(),
            a.network_graph.clone(),
            a.network_actor.clone(),
        );
        let mut source = a.get_channel_actor_state(id);
        let mut tlc = source.tlc_state.offered_tlcs.tlcs[0].clone();
        source.tlc_state.offered_tlcs.tlcs.clear();
        source
            .retryable_tlc_operations
            .push_back(fiber_types::RetryableTlcOperation::AddTlc(
                fiber_types::AddTlcCommand {
                    amount: tlc.amount,
                    payment_hash: tlc.payment_hash,
                    attempt_id: tlc.attempt_id,
                    expiry: tlc.expiry,
                    hash_algorithm: tlc.hash_algorithm,
                    onion_packet: tlc.onion_packet.clone(),
                    shared_secret: tlc.shared_secret,
                    previous_tlc: None,
                    is_trampoline_hop: tlc.is_trampoline_hop,
                },
            ));
        a.store.insert_channel_actor_state(source.clone());
        attempt.status = AttemptStatus::Retrying;
        attempt.tried_times = 1;
        assert!(actor.channel_owns_attempt(&attempt));
        source.state = fiber_types::ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
        a.store.insert_channel_actor_state(source.clone());
        assert!(
            !actor.channel_owns_attempt(&attempt),
            "a closed failure notification cannot execute the retry"
        );
        attempt.status = AttemptStatus::Created;
        assert!(
            actor.channel_owns_attempt(&attempt),
            "Created must still wait for the durable failure handoff"
        );
        attempt.status = AttemptStatus::Retrying;
        tlc.applied_flags.insert(fiber_types::AppliedFlags::REMOVE);
        source
            .session_v2
            .as_mut()
            .unwrap()
            .remove_effects
            .push((tlc, Some(attempt.tried_times)));
        a.store.insert_channel_actor_state(source.clone());
        assert!(
            actor.channel_owns_attempt(&attempt),
            "the current execution's outbox must prevent a duplicate retry"
        );
        source.session_v2.as_mut().unwrap().remove_effects[0].1 = Some(attempt.tried_times - 1);
        a.store.insert_channel_actor_state(source.clone());
        assert!(
            !actor.channel_owns_attempt(&attempt),
            "an older failed execution must not prevent retrying"
        );
        source.session_v2.as_mut().unwrap().remove_effects[0].1 = None;
        a.store.insert_channel_actor_state(source.clone());
        assert!(
            actor.channel_owns_attempt(&attempt),
            "an unbound result must be resolved before retrying"
        );
        source.retryable_tlc_operations.clear();
        source.session_v2.as_mut().unwrap().remove_effects.clear();
        a.store.insert_channel_actor_state(source);
        attempt.last_error = Some("persisted failure awaiting retry".to_owned());
        a.store.insert_attempt(attempt.clone());
        let (mailbox, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
        let mut state = PaymentActorState::new(PaymentActorArguments {
            payment_hash: hash,
            init_command: PaymentActorMessage::RetrySendPayment(None),
        });
        actor.register_payment_retry(mailbox.clone(), &mut state, Some(attempt.id));
        // The stopped transport is intentional: a duplicate startup claim must do no network work.
        actor
            .handle_command(
                mailbox.clone(),
                &mut state,
                PaymentActorMessage::ResumeAfterClosedChannel(attempt.id),
            )
            .await
            .unwrap();
        assert_eq!(
            a.store.get_attempt(hash, attempt.id).unwrap().status,
            AttemptStatus::Retrying,
            "startup must not dispatch again while a live callback's retry is scheduled"
        );
        mailbox.stop(None);
        task.await.unwrap();
    }
}
