use std::collections::HashMap;
use std::sync::Arc;

use super::network::{SendOnionPacketCommand, SendPaymentResponse, ASSUME_NETWORK_MYSELF_ALIVE};
use super::types::{Hash256, Privkey, Pubkey, TlcErrData};
use crate::fiber::channel::{ChannelActorStateStore, PrevTlcInfo, ProcessingChannelError};
use crate::fiber::config::{
    DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_MAX_PARTS, MAX_PAYMENT_TLC_EXPIRY_LIMIT,
    MIN_TLC_EXPIRY_DELTA, PAYMENT_MAX_PARTS_LIMIT,
};
use crate::fiber::fee::{calculate_fee_with_base, calculate_tlc_forward_fee};
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{
    GraphChannelStat, NetworkGraph, NetworkGraphStateStore, PathFindError, RouterHop,
};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::{
    NetworkActorStateStore, DEFAULT_CHAIN_ACTOR_TIMEOUT, DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT,
    DEFAULT_PAYMENT_TRY_LIMIT, MAX_CUSTOM_RECORDS_SIZE,
};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::U128Hex;
use crate::fiber::types::{
    BasicMppPaymentData, BroadcastMessageWithTimestamp, PaymentHopData, PeeledPaymentOnionPacket,
    RemoveTlcReason, TlcErr, TlcErrorCode,
};
use crate::fiber::{
    KeyPair, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::invoice::{CkbInvoice, InvoiceStore, PreimageStore};
use crate::Error;
use crate::{debug_event, now_timestamp_as_millis_u64};
use ckb_hash::blake2b_256;
use ckb_types::packed::{OutPoint, Script};
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
const MAX_FEE_RATE_DENOMINATOR: u128 = 1000;

/// The status of a payment, will update as the payment progresses.
/// The transfer path for payment status is `Created -> Inflight -> Success | Failed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    /// initial status, a payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success,
    /// related HTLC is failed
    Failed,
}

impl PaymentStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, PaymentStatus::Success | PaymentStatus::Failed)
    }
}

/// The node and channel information in a payment route hop
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// the public key of the node
    pub pubkey: Pubkey,
    /// the amount for this hop
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// the channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

/// The router is a list of nodes that the payment will go through.
/// We store in the payment session and then will use it to track the payment history.
/// The router is a list of nodes that the payment will go through.
/// For example:
///    `A(amount, channel) -> B -> C -> D`
/// means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// the nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

impl SessionRoute {
    // Create a new route from the source to the target with the given payment hops.
    // The payment hops are the hops that the payment will go through.
    // for a payment route A -> B -> C -> D
    // the `payment_hops` is [B, C, D], which is a convenient way for onion routing.
    // here we need to create a session route with source, which is A -> B -> C -> D
    pub fn new(source: Pubkey, target: Pubkey, payment_hops: &[PaymentHopData]) -> Self {
        //dbg!(payment_hops);
        let nodes = std::iter::once(source)
            .chain(
                payment_hops
                    .iter()
                    .map(|hop| hop.next_hop.unwrap_or(target)),
            )
            .zip(payment_hops)
            .map(|(pubkey, hop)| SessionRouteNode {
                pubkey,
                channel_outpoint: OutPoint::new(
                    if hop.funding_tx_hash != Hash256::default() {
                        hop.funding_tx_hash.into()
                    } else {
                        Hash256::default().into()
                    },
                    0,
                ),
                amount: hop.amount,
            })
            .collect();
        Self { nodes }
    }

    pub fn receiver_amount(&self) -> u128 {
        self.nodes.last().map_or(0, |s| s.amount)
    }

    pub fn fee(&self) -> u128 {
        let first_amount = self.nodes.first().map_or(0, |s| s.amount);
        let last_amount = self.receiver_amount();
        debug_assert!(first_amount >= last_amount);
        first_amount - last_amount
    }

    pub(crate) fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.nodes
            .iter()
            .map(|x| (x.pubkey, &x.channel_outpoint, x.amount))
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentData {
    pub target_pubkey: Pubkey,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub invoice: Option<String>,
    pub final_tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub timeout: Option<u64>,
    pub max_fee_amount: Option<u128>,
    /// The number of parts for the payment, only used for multi-part payment
    pub max_parts: Option<u64>,
    pub keysend: bool,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub preimage: Option<Hash256>,
    pub custom_records: Option<PaymentCustomRecords>,
    pub allow_self_payment: bool,
    pub hop_hints: Vec<HopHint>,
    pub router: Vec<RouterHop>,
    /// This flag indicates the invoice whether to allow multi-path payment.
    pub allow_mpp: bool,
    pub dry_run: bool,

    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,
    #[serde(default)]
    pub trampoline_context: Option<TrampolineContext>,
    #[serde(skip)]
    pub channel_stats: GraphChannelStat,
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
    channel_stats: GraphChannelStat,
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

    pub fn channel_stats(mut self, channel_stats: GraphChannelStat) -> Self {
        self.channel_stats = channel_stats;
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
            if hops.iter().any(|h| *h == target_pubkey) {
                return Err("trampoline_hops must not contain target_pubkey".to_string());
            }
            let mut uniq = hops.clone();
            uniq.sort();
            uniq.dedup();
            if uniq.len() != hops.len() {
                return Err("trampoline_hops must not contain duplicates".to_string());
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
            if custom_records.data.values().map(|v| v.len()).sum::<usize>()
                > MAX_CUSTOM_RECORDS_SIZE
            {
                return Err(format!(
                    "the sum size of custom_records's value can not more than {} bytes",
                    MAX_CUSTOM_RECORDS_SIZE
                ));
            }
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
            channel_stats: self.channel_stats,
            trampoline_context: self.trampoline_context,
        })
    }
}

impl SendPaymentData {
    pub fn new(command: SendPaymentCommand) -> Result<SendPaymentData, String> {
        let invoice = command
            .invoice
            .as_ref()
            .map(|invoice| invoice.parse::<CkbInvoice>())
            .transpose()
            .map_err(|_| "invoice is invalid".to_string())?;

        if let Some(invoice) = invoice.clone() {
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
        let final_tlc_expiry_delta = invoice
            .as_ref()
            .and_then(|i| i.final_tlc_minimum_expiry_delta().copied())
            .or(command.final_tlc_expiry_delta)
            .unwrap_or(DEFAULT_FINAL_TLC_EXPIRY_DELTA);

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
            let mut rng = rand::thread_rng();
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
            .channel_stats(Default::default())
            .build()
    }

    pub fn max_parts(&self) -> usize {
        self.max_parts.unwrap_or(DEFAULT_MAX_PARTS) as usize
    }

    pub fn allow_mpp(&self) -> bool {
        // only allow mpp if max_parts is greater than 1 and not keysend
        self.allow_mpp && self.max_parts() > 1 && !self.keysend
    }

    pub fn use_trampoline_routing(&self) -> bool {
        self.trampoline_hops
            .as_ref()
            .is_some_and(|hops| !hops.is_empty())
    }

    pub fn trampoline_hops(&self) -> Option<&[Pubkey]> {
        self.trampoline_hops.as_deref()
    }

    pub fn payment_invoice(&self) -> Option<CkbInvoice> {
        self.invoice
            .as_ref()
            .and_then(|x| x.parse::<CkbInvoice>().ok())
    }

    pub fn hash_algorithm(&self) -> HashAlgorithm {
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

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_code: Option<TlcErrorCode>,
    // For non-MPP, this is the maximum number of single attempt retry limit
    // For MPP, this is the sum limit of all parts' retry times.
    pub try_limit: u32,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    #[serde(skip)]
    pub cached_attempts: Vec<Attempt>, // Add a cache for attempts
}

impl PaymentSession {
    pub fn new(
        store: &impl NetworkGraphStateStore,
        request: SendPaymentData,
        try_limit: u32,
    ) -> Self {
        let now = now_timestamp_as_millis_u64();
        Self {
            request,
            last_error: None,
            last_error_code: None,
            try_limit,
            status: PaymentStatus::Created,
            created_at: now,
            last_updated_at: now,
            cached_attempts: vec![],
        }
        .init_attempts(store)
    }

    pub fn init_attempts(mut self, store: &impl NetworkGraphStateStore) -> Self {
        self.flush_attempts(store);
        self
    }

    pub fn flush_attempts(&mut self, store: &impl NetworkGraphStateStore) {
        self.cached_attempts = store.get_attempts(self.request.payment_hash);
        self.status = self.calc_payment_status();
    }

    pub fn update_with_attempt(&mut self, attempt: Attempt) {
        if let Some(a) = self.cached_attempts.iter_mut().find(|a| a.id == attempt.id) {
            *a = attempt;
        }
        self.status = self.calc_payment_status();
    }

    pub fn retry_times(&self) -> u32 {
        self.attempts().map(|a| a.tried_times).sum()
    }

    pub fn allow_mpp(&self) -> bool {
        self.request.allow_mpp()
    }

    pub fn payment_hash(&self) -> Hash256 {
        self.request.payment_hash
    }

    pub fn is_payment_with_router(&self) -> bool {
        !self.request.router.is_empty()
    }

    pub fn is_dry_run(&self) -> bool {
        self.request.dry_run
    }

    pub fn attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.cached_attempts.iter()
    }

    #[cfg(test)]
    pub fn all_attempts_with_status(&self) -> Vec<(u64, AttemptStatus, Option<String>, u32, u128)> {
        self.cached_attempts
            .iter()
            .map(|a| {
                (
                    a.id,
                    a.status,
                    a.last_error.clone(),
                    a.tried_times,
                    a.route.receiver_amount(),
                )
            })
            .collect()
    }

    pub fn attempts_count(&self) -> usize {
        self.cached_attempts.len()
    }

    pub fn max_parts(&self) -> usize {
        if self.allow_mpp() && !self.request.use_trampoline_routing() {
            self.request.max_parts()
        } else {
            1
        }
    }

    pub fn active_attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.attempts().filter(|a| a.is_active())
    }

    pub fn fee_paid(&self) -> u128 {
        if self.request.use_trampoline_routing() {
            // For trampoline routing, calculate the actual total fee
            // which includes both trampoline service fee and routing fee
            let total_sent: u128 = self
                .active_attempts()
                .map(|a| {
                    // The first node's amount is what we send
                    a.route.nodes.first().map_or(0, |n| n.amount)
                })
                .sum();
            let total_received = self.request.amount;
            total_sent.saturating_sub(total_received)
        } else {
            // For normal routing, sum up the fee from each route
            self.active_attempts().map(|a| a.route.fee()).sum()
        }
    }

    fn success_attempts_amount_is_enough(&self) -> bool {
        let success_amount: u128 = self
            .attempts()
            .filter_map(|a| {
                if a.is_success() {
                    Some(a.route.receiver_amount())
                } else {
                    None
                }
            })
            .sum();
        success_amount >= self.request.amount
    }

    pub fn remain_fee_amount(&self) -> Option<u128> {
        let max_fee_amount = self.request.max_fee_amount?;
        let remain_fee = max_fee_amount.saturating_sub(self.fee_paid());
        Some(remain_fee)
    }

    pub fn remain_amount(&self) -> u128 {
        let sent_amount = self
            .active_attempts()
            .map(|a| a.route.receiver_amount())
            .sum::<u128>();
        self.request.amount.saturating_sub(sent_amount)
    }

    pub fn new_attempt(
        &self,
        attempt_id: u64,
        source: Pubkey,
        target: Pubkey,
        route_hops: Vec<PaymentHopData>,
    ) -> Attempt {
        let now = now_timestamp_as_millis_u64();
        let payment_hash = self.payment_hash();
        // For HTLC, the attempt hash is the payment hash
        let hash = payment_hash;
        let try_limit = if self.allow_mpp() {
            DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT
        } else {
            self.try_limit
        };

        let route = SessionRoute::new(source, target, &route_hops);

        Attempt {
            id: attempt_id,
            hash,
            try_limit,
            tried_times: 1,
            payment_hash,
            route,
            route_hops,
            session_key: [0; 32],
            preimage: None,
            created_at: now,
            last_updated_at: now,
            last_error: None,
            status: AttemptStatus::Created,
        }
    }

    pub fn append_attempt(&mut self, attempt: Attempt) {
        self.cached_attempts.push(attempt);
    }

    pub fn allow_more_attempts(&self) -> bool {
        if self.status.is_final() {
            return false;
        }

        if self.remain_amount() == 0 {
            // no remaining amount, imply no need to retry
            return false;
        }

        if self.retry_times() >= self.try_limit {
            // already reached the retry limit, no more attempts allowed
            return false;
        }

        if self.active_attempts().count() >= self.max_parts() {
            return false;
        }

        // otherwise, should continue retry
        true
    }

    pub fn calc_payment_status(&self) -> PaymentStatus {
        if self.cached_attempts.is_empty() || self.status.is_final() {
            return self.status;
        }

        if self.attempts().any(|a| a.is_inflight()) {
            // if any attempt is created or inflight, the payment is inflight
            return PaymentStatus::Inflight;
        }

        if self.attempts().all(|a| a.is_failed()) && !self.allow_more_attempts() {
            return PaymentStatus::Failed;
        }

        if self.success_attempts_amount_is_enough() {
            return PaymentStatus::Success;
        }

        return PaymentStatus::Created;
    }

    fn set_status(&mut self, status: PaymentStatus) {
        self.status = status;
        self.last_updated_at = now_timestamp_as_millis_u64();
    }

    pub fn set_inflight_status(&mut self) {
        self.set_status(PaymentStatus::Inflight);
    }

    pub fn set_success_status(&mut self) {
        self.set_status(PaymentStatus::Success);
        self.last_error = None;
        self.last_error_code = None;
    }

    pub fn set_failed_status(&mut self, error: &str) {
        self.set_status(PaymentStatus::Failed);
        self.last_error = Some(error.to_string());
    }
}

impl From<PaymentSession> for SendPaymentResponse {
    fn from(session: PaymentSession) -> Self {
        let status = session.status;
        let fee = session.fee_paid();
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

/// The status of a payment attempt, will update as the payment progresses.
/// The transfer path for attempt status is:
///
///    Created --> Inflight ----> Success
//                 /   |
///               /    |
///              |     | ----- no retry ----> Failed
///               \    |
///                \  retry
///                 \  |
///                  \ |
///                  Retrying
///
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AttemptStatus {
    /// initial status, a payment attempt is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// the attempt is retrying after failed
    Retrying,
    /// related HTLC is successfully settled
    Success,
    /// related HTLC is failed
    Failed,
}

impl AttemptStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, AttemptStatus::Success | AttemptStatus::Failed)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: u64,
    pub try_limit: u32,
    pub tried_times: u32,
    pub hash: Hash256,
    pub status: AttemptStatus,
    pub payment_hash: Hash256,
    pub route: SessionRoute,
    pub route_hops: Vec<PaymentHopData>,
    pub session_key: [u8; 32],
    pub preimage: Option<Hash256>,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub last_error: Option<String>,
}

impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt")
            .field("id", &self.id)
            .field("try_limit", &self.try_limit)
            .field("tried_times", &self.tried_times)
            .field("hash", &self.hash)
            .field("status", &self.status)
            .field("payment_hash", &self.payment_hash)
            .field("route", &self.route)
            .field("route_hops", &self.route_hops)
            .field("session_key", &"[REDACTED]")
            .field("preimage", &self.preimage.as_ref().map(|_| "[REDACTED]"))
            .field("created_at", &self.created_at)
            .field("last_updated_at", &self.last_updated_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl Attempt {
    pub fn set_inflight_status(&mut self) {
        self.status = AttemptStatus::Inflight;
        self.last_error = None;
    }

    pub fn set_success_status(&mut self) {
        self.status = AttemptStatus::Success;
        self.last_error = None;
    }

    pub fn set_failed_status(&mut self, error: &str, retryable: bool) {
        self.last_error = Some(error.to_string());
        self.last_updated_at = now_timestamp_as_millis_u64();
        if !retryable || self.tried_times > self.try_limit {
            self.status = AttemptStatus::Failed;
        } else {
            self.status = AttemptStatus::Retrying;
            self.tried_times += 1;
        }
    }

    pub fn update_route(&mut self, new_route_hops: Vec<PaymentHopData>) {
        self.route_hops = new_route_hops;
        let sender = self.route.nodes[0].pubkey;
        let receiver = self.route.nodes.last().unwrap().pubkey;
        self.route = SessionRoute::new(sender, receiver, &self.route_hops);
    }

    pub fn is_success(&self) -> bool {
        self.status == AttemptStatus::Success
    }

    pub fn is_inflight(&self) -> bool {
        self.status == AttemptStatus::Inflight
    }

    pub fn is_failed(&self) -> bool {
        self.status == AttemptStatus::Failed
    }

    pub fn is_active(&self) -> bool {
        self.status != AttemptStatus::Failed
    }

    pub fn is_retrying(&self) -> bool {
        self.status == AttemptStatus::Retrying
    }

    pub fn first_hop_channel_outpoint_eq(&self, out_point: &OutPoint) -> bool {
        self.route
            .nodes
            .first()
            .map(|x| x.channel_outpoint.eq(out_point))
            .unwrap_or_default()
    }

    pub(crate) fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.route.channel_outpoints()
    }

    pub fn hops_public_keys(&self) -> Vec<Pubkey> {
        // Skip the first node, which is the sender.
        self.route.nodes.iter().skip(1).map(|x| x.pubkey).collect()
    }
}

/// A hop hint is a hint for a node to use a specific channel,
/// will usually used for the last hop to the target node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node
    pub(crate) pubkey: Pubkey,
    /// The outpoint for the channel
    #[serde_as(as = "EntityHex")]
    pub(crate) channel_outpoint: OutPoint,
    /// The fee rate to use this hop to forward the payment.
    pub(crate) fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    pub(crate) tlc_expiry_delta: u64,
}

#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrampolineHop {
    pub pubkey: Pubkey,
}

impl TrampolineHop {
    pub fn new(pubkey: Pubkey) -> Self {
        Self { pubkey }
    }
}

// 0 ~ 65535 is reserved for endpoint usage, index aboving 65535 is reserved for internal usage
pub const USER_CUSTOM_RECORDS_MAX_INDEX: u32 = 65535;
/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    pub data: HashMap<u32, Vec<u8>>,
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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TrampolineContext {
    /// Optional final trampoline onion packet.
    ///
    /// When provided, this onion packet will be attached to the payload of the next hop
    /// (which in this context is the next trampoline node).
    pub remaining_trampoline_onion: Vec<u8>,
    /// Previous TLCs information for the payment session.
    /// This is used to associate the outgoing payment with the incoming payment.
    pub previous_tlcs: Vec<PrevTlcInfo>,
    /// Hash algorighm used for the payment.
    pub hash_algorithm: HashAlgorithm,
    /// available max fee amount for trampoline forwarding
    pub available_fee_amount: u128,
}

impl SendPaymentCommand {
    pub fn build_send_payment_data(self) -> Result<SendPaymentData, Error> {
        SendPaymentData::new(self).map_err(|e| {
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
    pub fn build_send_payment_data(self, source: Pubkey) -> Result<SendPaymentData, Error> {
        // Only proceed if we have at least one hop requirement
        let Some(last_edge) = self.router.last() else {
            return Err(Error::InvalidParameter(
                "No hop requirements provided".to_string(),
            ));
        };

        // let source = self.network_graph.read().await.get_source_pubkey();
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
            ..Default::default()
        };

        let mut payment_data = SendPaymentData::new(payment_command).map_err(|e| {
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

#[derive(Debug, AsRefStr)]
pub enum PaymentActorMessage {
    // SendPayment
    SendPayment(
        SendPaymentData,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    RetrySendPayment(Option<u64>),
    OnAddTlcResultEvent {
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
    },
    OnRemoveTlcEvent {
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
    },
    /// Periodic check to detect stuck payments and log status
    CheckPaymentStatus,
}

pub struct PaymentActorState {
    payment_hash: Hash256,
    init_command: Option<PaymentActorMessage>,

    // the number of pending retrying send payments, we track it for
    // set retry delay dynamically, pending too many payments may have a negative impact
    // on the node performance, which in worst case may lead node not response revoke_and_ack
    // in expected time, and then the peer will disconnect us.
    retry_send_payment_count: usize,
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
            retry_send_payment_count: 0,
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

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
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
                NetworkActorEvent::PaymentActorStopped(state.payment_hash),
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
        request: SendPaymentData,
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
        request: SendPaymentData,
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
        match command {
            PaymentActorMessage::SendPayment(payment_request, reply) => {
                match self
                    .on_send_payment(myself.clone(), state, payment_request)
                    .await
                {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                        self.check_payment_final(myself, state);
                    }
                    Err(e) => {
                        error!("Failed to send payment: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));

                        // stop actor
                        myself.stop(Some("Failed to send payment".to_string()));
                    }
                }
            }
            PaymentActorMessage::RetrySendPayment(attempt_id) => {
                let _ = self
                    .resume_payment_session(myself.clone(), state, attempt_id)
                    .await;
                self.check_payment_final(myself, state);
            }
            PaymentActorMessage::OnAddTlcResultEvent {
                attempt_id,
                add_tlc_result,
            } => {
                self.handle_add_tlc_result_event(myself.clone(), state, attempt_id, add_tlc_result)
                    .await;
                self.check_payment_final(myself, state);
            }
            PaymentActorMessage::OnRemoveTlcEvent { attempt_id, reason } => {
                self.handle_remove_tlc_event(myself.clone(), state, attempt_id, reason)
                    .await;
                self.check_payment_final(myself, state);
            }
            PaymentActorMessage::CheckPaymentStatus => {
                self.handle_check_payment_status(myself, state);
            }
        };
        Ok(())
    }

    /// Handle periodic status check to detect stuck payments
    fn handle_check_payment_status(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
    ) {
        let payment_hash = state.payment_hash;
        if let Some(session) = self.store.get_payment_session(payment_hash) {
            if session.status.is_final() {
                // Payment has reached final status, stop the actor
                myself.stop(Some(format!(
                    "Payment complete with status {:?}",
                    session.status
                )));
            } else {
                // Payment is still not final, log the current status for debugging
                let active_attempts = session.active_attempts().count();
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

    fn check_payment_final(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
    ) {
        if let Some(session) = self.store.get_payment_session(state.payment_hash) {
            if session.status.is_final() {
                myself.stop(Some(format!(
                    "Payment complete with status {:?}",
                    session.status
                )));
            }
        } else {
            error!("Can't find payment session");
            myself.stop(Some("Can't find payment session".to_string()));
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
            // `session.remain_amount()` do not contains this part of amount,
            // so we need to add the receiver amount to it, so we may make fewer
            // attempts to send the payment.
            let amount = session.remain_amount() + attempt.route.receiver_amount();
            let max_fee = session.remain_fee_amount();
            let channel_stats = {
                let graph = self.network_graph.read().await;
                graph.channel_stats()
            };

            session.request.channel_stats = GraphChannelStat::new(Some(channel_stats));
            #[cfg(not(target_arch = "wasm32"))]
            let hops = self
                .build_route_in_spawn_task(amount, None, max_fee, session.request.clone())
                .await
                .map_err(|e| {
                    Error::BuildPaymentRouteError(format!("Failed to build route, {}", e))
                })?;

            #[cfg(target_arch = "wasm32")]
            let hops = self
                .build_route(amount, None, max_fee, session.request.clone())
                .await
                .map_err(|e| {
                    Error::BuildPaymentRouteError(format!("Failed to build route, {}", e))
                })?;

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

        session.request.channel_stats = GraphChannelStat::new(Some(channel_stats));
        let amount_low_bound = Some(1);
        let mut attempt_id = session.attempts_count() as u64;
        let mut target_amount = remain_amount;
        let mut single_path_max = None;
        let mut iteration = 0;

        if session.max_parts() > 1 && !is_self_pay {
            let path_max = {
                let graph = self.network_graph.read().await;
                graph.find_path_max_capacity(&session.request)?
            };
            if path_max * (session.max_parts() as u128) < remain_amount {
                let error = "Failed to build enough routes for MPP payment".to_string();
                return Err(Error::SendPaymentError(error));
            }
            single_path_max = Some(path_max);
        }

        while (result.len() < session.max_parts() - active_parts) && remain_amount > 0 {
            iteration += 1;

            debug!(
                "build route iteration {}, target_amount: {} amount_low_bound: {:?} remain_amount: {}, max_parts: {}, max_fee: {:?}",
                iteration,
                target_amount,
                amount_low_bound,
                remain_amount,
                session.max_parts(),
                max_fee,
            );
            #[cfg(not(target_arch = "wasm32"))]
            let build_route_result = self
                .build_route_in_spawn_task(
                    target_amount,
                    amount_low_bound,
                    max_fee,
                    session.request.clone(),
                )
                .await;
            #[cfg(target_arch = "wasm32")]
            let build_route_result = self
                .build_route(
                    target_amount,
                    amount_low_bound,
                    max_fee,
                    session.request.clone(),
                )
                .await;

            match build_route_result {
                Err(e) => {
                    error!("Here failed to build route: {}", e);
                    let error = format!("Failed to build route, {}", e);
                    return Err(Error::SendPaymentError(error));
                }
                Ok(mut hops) => {
                    assert_ne!(hops[0].funding_tx_hash, Hash256::default());
                    self.apply_trampoline_forwarding_fee(session, source, &hops, &mut max_fee)
                        .await?;

                    // Embed trampoline onion in the last hop if available
                    // This is needed both for actual payments and dry_run to get correct fee calculations
                    if let Some(trampoline) = &session.request.trampoline_context {
                        if let Some(last_hop) = hops.last_mut() {
                            last_hop.trampoline_onion =
                                Some(trampoline.remaining_trampoline_onion.clone());
                        }
                    }

                    let new_attempt_id = if session.is_dry_run() {
                        0
                    } else {
                        attempt_id += 1;
                        attempt_id
                    };

                    let attempt = session.new_attempt(
                        new_attempt_id,
                        source,
                        session.request.target_pubkey,
                        hops,
                    );

                    let session_route = &attempt.route;
                    #[cfg(debug_assertions)]
                    dbg!(
                        "remain amount: {}, minimal_amount: {} receiver amount: {}",
                        remain_amount,
                        target_amount,
                        session_route.receiver_amount()
                    );

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
                                session.request.channel_stats.add_channel(
                                    channel_outpoint,
                                    sent_node,
                                    *amount,
                                );
                            }
                        }
                    }
                    let current_amount = session_route.receiver_amount();
                    if session.request.use_trampoline_routing() {
                        remain_amount -= target_amount;
                    } else {
                        remain_amount -= current_amount;
                    }
                    target_amount = if let Some(single) = single_path_max {
                        single.min(remain_amount)
                    } else {
                        remain_amount
                    };
                    if let Some(fee) = max_fee {
                        if session.request.trampoline_context.is_some() && fee < session_route.fee()
                        {
                            return Err(Error::SendPaymentError(
                                "Trampoline forwarding fee insufficient".to_string(),
                            ));
                        }
                        max_fee = Some(fee - session_route.fee());
                    }
                    result.push(attempt);
                    if remain_amount > 0
                        && remain_amount
                            > current_amount * (session.max_parts() - result.len()) as u128
                    {
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
            let channel_info = graph.get_channel(&channel_outpoint).ok_or_else(|| {
                Error::SendPaymentError(format!(
                    "Trampoline forwarding channel not found: {:?}",
                    channel_outpoint
                ))
            })?;
            let update = if channel_info.node1 == source {
                channel_info.update_of_node1.as_ref()
            } else if channel_info.node2 == source {
                channel_info.update_of_node2.as_ref()
            } else {
                None
            }
            .ok_or_else(|| {
                Error::SendPaymentError(format!(
                    "Trampoline forwarding channel update missing: {:?}",
                    channel_outpoint
                ))
            })?;
            update.fee_rate
        };

        let local_fee =
            calculate_tlc_forward_fee(first_hop.amount, fee_rate as u128).map_err(|err| {
                Error::SendPaymentError(format!(
                    "Trampoline forwarding fee calculation failed: {:?}",
                    err
                ))
            })?;

        let fee_budget = max_fee.ok_or_else(|| {
            Error::SendPaymentError("Trampoline forwarding requires max_fee_amount".to_string())
        })?;
        if fee_budget < local_fee {
            session.last_error_code = Some(TlcErrorCode::FeeInsufficient);
            error!(
                "not enough fee budget for trampoline forwarding: budget {}, required {}",
                fee_budget, local_fee
            );
            return Err(Error::SendPaymentError(
                "Trampoline forwarding fee insufficient".to_string(),
            ));
        }
        *max_fee = Some(fee_budget - local_fee);
        Ok(())
    }

    async fn send_payment_onion_packet(
        &self,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
    ) -> Result<(), Error> {
        let session_key = Privkey::from_slice(KeyPair::generate_random_key().as_ref());
        assert_ne!(attempt.route_hops[0].funding_tx_hash, Hash256::default());

        attempt.session_key.copy_from_slice(session_key.as_ref());

        let peeled_onion_packet = match PeeledPaymentOnionPacket::create(
            session_key,
            attempt.route_hops.clone(),
            Some(attempt.hash.as_ref().to_vec()),
            &Secp256k1::signing_only(),
        ) {
            Ok(packet) => packet,
            Err(e) => {
                let err = format!(
                    "Failed to create onion packet: {:?}, error: {:?}",
                    attempt.hash, e
                );
                self.set_attempt_fail_with_error(session, attempt, &err, false);
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
                self.set_attempt_fail_with_error(session, attempt, &err, need_to_retry);
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
        session.set_failed_status(error);
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
        error: &str,
        retryable: bool,
    ) {
        if !retryable && !session.active_attempts().any(|a| a.id != attempt.id) {
            self.set_payment_fail_with_error(session, error, None);
        }

        attempt.set_failed_status(error, retryable);
        if !session.is_dry_run() {
            self.store.insert_attempt(attempt.clone());
        }
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
                self.set_attempt_fail_with_error(session, attempt, &err.to_string(), false);
                return Err(err);
            }
        }
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

        if session.status.is_final() {
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
        let Some(attempt_id) = attempt_id else {
            return Ok(());
        };

        match self.store.get_attempt(session.payment_hash(), attempt_id) {
            Some(mut attempt) if attempt.is_retrying() => {
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
                            &err.to_string(),
                            false,
                        );
                        return Err(err);
                    }
                    _ => {}
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

    async fn handle_add_tlc_result_event(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
    ) {
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
                    &error.to_string(),
                    need_to_retry,
                );

                if attempt.is_retrying() {
                    self.register_payment_retry(myself.clone(), state, Some(attempt.id));
                }
            }
        }
    }

    async fn handle_remove_tlc_event(
        &self,
        myself: ActorRef<PaymentActorMessage>,
        state: &mut PaymentActorState,
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
    ) {
        let payment_hash = state.payment_hash;
        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, attempt_id)
        else {
            error!(
                "Payment session or attempt not found for payment hash: {:?}, attempt id: {:?}",
                payment_hash, attempt_id
            );
            return;
        };

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
                }
            }
            RemoveTlcReason::RemoveTlcFail(reason) => {
                let error_detail = reason
                    .decode(&attempt.session_key, attempt.hops_public_keys())
                    .unwrap_or_else(|| {
                        debug_event!(self.network, "InvalidOnionError");
                        TlcErr::new(TlcErrorCode::InvalidOnionError)
                    });
                let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                    &attempt,
                    error_detail.clone(),
                    false,
                );
                debug!(
                    "payment_hash: {:?} set attempt failed with: {:?} need_to_retry: {:?}",
                    payment_hash,
                    error_detail.error_code.as_ref(),
                    need_to_retry
                );

                self.set_attempt_fail_with_error(
                    &mut session,
                    &mut attempt,
                    error_detail.error_code.as_ref(),
                    need_to_retry,
                );

                if attempt.is_retrying() {
                    self.register_payment_retry(myself.clone(), state, Some(attempt.id));
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
    }

    fn payment_need_more_retry(&self, session: &mut PaymentSession) -> Result<bool, Error> {
        session.flush_attempts(&self.store);
        let more_attempt = session.allow_more_attempts();
        if !more_attempt && session.remain_amount() > 0 {
            let err = "Can not send payment with limited attempts";
            self.set_payment_fail_with_error(session, err, None);
            return Err(Error::SendPaymentError(err.to_string()));
        }
        Ok(more_attempt)
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
        state.retry_send_payment_count += 1;
        let delay = (state.retry_send_payment_count as u64) * 20_u64;
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
                if payment_session.attempts().any(|a| a.is_inflight()) {
                    return Err(Error::InvalidParameter(format!(
                        "Payment session {} has attempts that are in flight state, can not retry",
                        payment_data.payment_hash
                    )));
                }
                if !payment_data.dry_run {
                    self.store.delete_attempts(payment_data.payment_hash);
                }
            }
        }

        // for dry run, we only build the route and return the hops info,
        // will not store the payment session and send the onion packet
        if payment_data.dry_run {
            let mut payment_session = PaymentSession::new(&self.store, payment_data, 0);
            self.build_payment_routes(&mut payment_session).await?;
            return Ok(payment_session.into());
        }

        let try_limit = if payment_data.allow_mpp() {
            payment_data.max_parts() as u32 * DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT
        } else {
            DEFAULT_PAYMENT_TRY_LIMIT
        };
        let mut payment_session = PaymentSession::new(&self.store, payment_data, try_limit);
        assert!(payment_session.attempts_count() == 0);
        self.store.insert_payment_session(payment_session.clone());

        self.resume_payment_session(myself, state, None).await?;
        payment_session.flush_attempts(&self.store);
        return Ok(payment_session.into());
    }
}
