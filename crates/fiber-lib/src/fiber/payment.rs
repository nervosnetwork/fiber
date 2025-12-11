use std::collections::HashMap;
use std::sync::Arc;

use super::network::{
    SendOnionPacketCommand, SendPaymentData, SendPaymentResponse, ASSUME_NETWORK_MYSELF_ALIVE,
};
use super::types::{Hash256, Privkey, Pubkey, TlcErrData};
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{GraphChannelStat, NetworkGraph, NetworkGraphStateStore, RouterHop};
use crate::fiber::network::{
    NetworkActorStateStore, DEFAULT_CHAIN_ACTOR_TIMEOUT, DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT,
    DEFAULT_PAYMENT_TRY_LIMIT,
};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::U128Hex;
use crate::fiber::types::{
    BroadcastMessageWithTimestamp, PaymentHopData, PeeledPaymentOnionPacket, TlcErr, TlcErrorCode,
};
use crate::fiber::{
    KeyPair, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::invoice::{InvoiceStore, PreimageStore};
use crate::now_timestamp_as_millis_u64;
use crate::Error;
use ckb_types::packed::{OutPoint, Script};
use ractor::{call_t, Actor, ActorProcessingErr};
use ractor::{concurrency::Duration, ActorRef, RpcReplyPort};
use secp256k1::Secp256k1;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use strum::AsRefStr;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, error};

/// The status of a payment, will update as the payment progresses.
/// The transfer path for payment status is `Created -> Inflight -> Success | Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
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
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub last_error: Option<String>,
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
        if self.allow_mpp() {
            self.request.max_parts()
        } else {
            1
        }
    }

    pub fn active_attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.attempts().filter(|a| a.is_active())
    }

    pub fn fee_paid(&self) -> u128 {
        self.active_attempts().map(|a| a.route.fee()).sum()
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

        #[cfg(debug_assertions)]
        {
            dbg!(&all_attempts);
            dbg!(
                fee,
                &status,
                all_attempts.len(),
                session.try_limit,
                &session.last_error
            );

            let active_count = session.active_attempts().count();
            let active_amount = session
                .active_attempts()
                .map(|a| a.route.receiver_amount())
                .sum::<u128>();
            dbg!(active_amount, session.request.amount, active_count);
        }

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

#[derive(Clone, Serialize, Deserialize, Debug)]
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
    // the maximum fee amounts in shannons that the sender is willing to pay, default is 1000 shannons CKB.
    pub max_fee_amount: Option<u128>,
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

#[derive(Debug, AsRefStr)]
pub enum PaymentActorMessage {
    // SendPayment
    SendPayment(
        SendPaymentData,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    RetrySendPayment(Option<u64>),
    // Check payment status, stop actor if payment is end
    CheckPayment,
}

pub enum PaymentActorInitCommand {
    Send(SendPaymentData),
    Resume(Hash256, Option<u64>),
}

pub struct PaymentActorState {
    payment_hash: Hash256,
    init_command: PaymentActorInitCommand,
    send_payment_reply: Option<RpcReplyPort<Result<SendPaymentResponse, String>>>,

    // the number of pending retrying send payments, we track it for
    // set retry delay dynamically, pending too many payments may have a negative impact
    // on the node performance, which in worst case may lead node not response revoke_and_ack
    // in expected time, and then the peer will disconnect us.
    retry_send_payment_count: usize,
}

impl PaymentActorState {
    pub fn new(args: PaymentActorArguments) -> Self {
        let PaymentActorArguments {
            init_command,
            send_payment_reply,
        } = args;
        let payment_hash = match &init_command {
            PaymentActorInitCommand::Send(payment_request) => payment_request.payment_hash,
            PaymentActorInitCommand::Resume(payment_hash, _) => *payment_hash,
        };
        Self {
            payment_hash,
            init_command,
            send_payment_reply,
            retry_send_payment_count: 0,
        }
    }
}

pub struct PaymentActorArguments {
    pub init_command: PaymentActorInitCommand,
    pub send_payment_reply: Option<RpcReplyPort<Result<SendPaymentResponse, String>>>,
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
        let reply = state.send_payment_reply.take().unwrap_or_else(|| {
            let (send, _recv) = oneshot::channel::<Result<_, _>>();
            RpcReplyPort::from(send)
        });
        let init_msg = match &state.init_command {
            PaymentActorInitCommand::Send(payment_request) => {
                PaymentActorMessage::SendPayment(payment_request.clone(), reply)
            }
            PaymentActorInitCommand::Resume(_payment_hash, attempt_id) => {
                PaymentActorMessage::RetrySendPayment(*attempt_id)
            }
        };
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
            PaymentActorMessage::CheckPayment => {
                self.check_payment_final(myself, state);
            }
        };
        Ok(())
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
            let graph = self.network_graph.read().await;

            session.request.channel_stats = GraphChannelStat::new(Some(graph.channel_stats()));

            let hops = graph
                .build_route(amount, None, max_fee, &session.request)
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
        let graph = self.network_graph.read().await;
        let source = graph.get_source_pubkey();
        let active_parts = session.attempts().filter(|a| a.is_active()).count();
        let is_self_pay = source == session.request.target_pubkey;
        let mut remain_amount = session.remain_amount();
        let mut max_fee = session.remain_fee_amount();
        let mut result = vec![];

        if remain_amount == 0 {
            let error = format!("Send amount {} is not expected to be 0", remain_amount);
            return Err(Error::SendPaymentError(error));
        }

        session.request.channel_stats = GraphChannelStat::new(Some(graph.channel_stats()));
        let amount_low_bound = Some(1);
        let mut attempt_id = session.attempts_count() as u64;
        let mut target_amount = remain_amount;
        let mut single_path_max = None;
        let mut iteration = 0;

        if session.max_parts() > 1 && !is_self_pay {
            let path_max = graph.find_path_max_capacity(&session.request)?;
            if path_max * (session.max_parts() as u128) < remain_amount {
                let error = "Failed to build enough routes for MPP payment".to_string();
                return Err(Error::SendPaymentError(error));
            }
            single_path_max = Some(path_max);
        }

        while (result.len() < session.max_parts() - active_parts) && remain_amount > 0 {
            iteration += 1;

            debug!(
                "build route iteration {}, target_amount: {} amount_low_bound: {:?} remain_amount: {}",
                iteration,
                target_amount,
                amount_low_bound,
                remain_amount,
            );
            match graph.build_route(target_amount, amount_low_bound, max_fee, &session.request) {
                Err(e) => {
                    let error = format!("Failed to build route, {}", e);
                    return Err(Error::SendPaymentError(error));
                }
                Ok(hops) => {
                    assert_ne!(hops[0].funding_tx_hash, Hash256::default());
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
                        "left amount: {}, minimal_amount: {} target amount: {}",
                        remain_amount - session_route.receiver_amount(),
                        target_amount,
                        session_route.receiver_amount()
                    );

                    for (from, channel_outpoint, amount) in session_route.channel_outpoints() {
                        if let Some(sent_node) = graph.get_channel_sent_node(channel_outpoint, from)
                        {
                            session.request.channel_stats.add_channel(
                                channel_outpoint,
                                sent_node,
                                amount,
                            );
                        }
                    }
                    let current_amount = session_route.receiver_amount();
                    remain_amount -= current_amount;
                    target_amount = if let Some(single) = single_path_max {
                        single.min(remain_amount)
                    } else {
                        remain_amount
                    };
                    if let Some(fee) = max_fee {
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

    fn set_payment_fail_with_error(&self, session: &mut PaymentSession, error: &str) {
        session.set_failed_status(error);
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
            self.set_payment_fail_with_error(session, error);
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
                self.set_payment_fail_with_error(&mut session, &e.to_string());
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

    fn payment_need_more_retry(&self, session: &mut PaymentSession) -> Result<bool, Error> {
        session.flush_attempts(&self.store);
        let more_attempt = session.allow_more_attempts();
        if !more_attempt && session.remain_amount() > 0 {
            let err = "Can not send payment with limited attempts";
            self.set_payment_fail_with_error(session, err);
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
