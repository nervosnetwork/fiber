use crate::fiber::graph::RouterHop;
use crate::fiber::network::BuildRouterCommand;
use crate::fiber::network::HopRequire;
use crate::fiber::payment::SendPaymentWithRouterCommand;
#[cfg(debug_assertions)]
use crate::fiber::payment::SessionRoute;
use crate::fiber::serde_utils::SliceHex;
use crate::fiber::serde_utils::U32Hex;
use crate::fiber::{
    channel::ChannelActorStateStore,
    payment::PaymentStatus,
    payment::{HopHint as NetworkHopHint, SendPaymentCommand},
    serde_utils::{EntityHex, U128Hex, U64Hex},
    types::{Hash256, Pubkey},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
use ckb_types::packed::OutPoint;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use serde_with::serde_as;
use std::collections::HashMap;

use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct GetPaymentCommandParams {
    /// The payment hash of the payment to retrieve
    pub payment_hash: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct GetPaymentCommandResult {
    /// The payment hash of the payment
    pub payment_hash: Hash256,
    /// The status of the payment
    pub status: PaymentStatus,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was created at, in milliseconds from UNIX epoch
    created_at: u64,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was last updated at, in milliseconds from UNIX epoch
    pub last_updated_at: u64,
    /// The error message if the payment failed
    pub failed_error: Option<String>,
    /// fee paid for the payment
    #[serde_as(as = "U128Hex")]
    pub fee: u128,

    /// The custom records to be included in the payment.
    pub custom_records: Option<PaymentCustomRecords>,

    #[cfg(debug_assertions)]
    /// The router is a list of nodes that the payment will go through.
    /// We store in the payment session and then will use it to track the payment history.
    /// The router is a list of nodes that the payment will go through.
    /// If the payment adapted MPP (multi-part payment), the routers will be a list of nodes
    /// For example:
    ///    `A(amount, channel) -> B -> C -> D`
    /// means A will send `amount` with `channel` to B.
    routers: Vec<SessionRoute>,
}

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, it's range limited in 0 ~ 65535, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
/// For example:
/// ```json
/// "custom_records": {
///    "0x1": "0x01020304",
///    "0x2": "0x05060708",
///    "0x3": "0x090a0b0c",
///    "0x4": "0x0d0e0f10010d090a0b0c"
///  }
/// ```
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    #[serde(flatten)]
    #[serde_as(as = "HashMap<U32Hex, SliceHex>")]
    pub data: HashMap<u32, Vec<u8>>,
}

impl From<PaymentCustomRecords> for crate::fiber::PaymentCustomRecords {
    fn from(records: PaymentCustomRecords) -> Self {
        crate::fiber::PaymentCustomRecords { data: records.data }
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommandParams {
    /// the identifier of the payment target
    pub target_pubkey: Option<Pubkey>,

    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set and there is a invoice, the amount will be set to the invoice amount
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,

    /// the TLC expiry limit for the whole payment, in milliseconds, each hop is with a default tlc delta of 1 day
    /// suppose the payment router is with N hops, the total tlc expiry limit is at least (N-1) days
    /// this is also the default value for the payment if this parameter is not provided
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_limit: Option<u64>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    #[serde_as(as = "Option<U64Hex>")]
    pub timeout: Option<u64>,

    /// the maximum fee amounts in shannons that the sender is willing to pay.
    /// Note: In trampoline routing mode, the sender will use the max_fee_amount as the total fee as much as possible.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_fee_amount: Option<u128>,

    /// the maximum fee rate per thousand (‰), default is 5 (0.5%)
    #[serde_as(as = "Option<U64Hex>")]
    pub max_fee_rate: Option<u64>,

    /// max parts for the payment, only used for multi-part payments
    #[serde_as(as = "Option<U64Hex>")]
    pub max_parts: Option<u64>,

    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    pub udt_type_script: Option<Script>,

    /// allow self payment, default is false
    pub allow_self_payment: Option<bool>,

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

    /// Optional route hints to reach the destination through private channels.
    /// Note:
    ///    1. this is only used for the private channels with the last hop.
    ///    2. `hop_hints` is only a `hint` for routing algorithm,
    ///       it is not a guarantee that the payment will be routed through the specified channels,
    ///       it is up to the routing algorithm to decide whether to use the hints or not.
    ///
    /// For example `(pubkey, channel_outpoint, fee_rate, tlc_expiry_delta)` suggest path router
    /// to use the channel of `channel_outpoint` at hop with `pubkey` to forward the payment
    /// and the fee rate is `fee_rate` and tlc_expiry_delta is `tlc_expiry_delta`.
    pub hop_hints: Option<Vec<HopHint>>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: Option<bool>,
}

/// A hop hint is a hint for a node to use a specific channel.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node
    pub pubkey: Pubkey,
    /// The outpoint of the channel
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,

    /// The fee rate to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub(crate) fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub(crate) tlc_expiry_delta: u64,
}

impl From<HopHint> for NetworkHopHint {
    fn from(hop_hint: HopHint) -> Self {
        NetworkHopHint {
            pubkey: hop_hint.pubkey,
            channel_outpoint: hop_hint.channel_outpoint,
            fee_rate: hop_hint.fee_rate,
            tlc_expiry_delta: hop_hint.tlc_expiry_delta,
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterParams {
    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set, the minimum routable amount `1` is used
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// udt type script for the payment router
    pub udt_type_script: Option<Script>,

    /// A list of hops that defines the route. This does not include the source hop pubkey.
    /// A hop info is a tuple of pubkey and the channel(specified by channel funding tx) will be used.
    /// This is a strong restriction given on payment router, which means these specified hops and channels
    /// must be adapted in the router. This is different from hop hints, which maybe ignored by find path.
    /// If channel is not specified, find path algorithm will pick a channel within these two peers.
    ///
    /// An error will be returned if there is no router could be build from given hops and channels
    pub hops_info: Vec<HopRequire>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,
}

/// The router returned by build_router
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BuildPaymentRouterResult {
    /// The hops information for router
    router_hops: Vec<RouterHop>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct SendPaymentWithRouterParams {
    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// The router to use for the payment
    pub router: Vec<RouterHop>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data.
    /// Limits: the sum size of values can not exceed 2048 bytes.
    ///
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
    pub udt_type_script: Option<Script>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: Option<bool>,
}

/// RPC module for channel management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait PaymentRpc {
    /// Sends a payment to a peer.
    #[method(name = "send_payment")]
    async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Retrieves a payment.
    #[method(name = "get_payment")]
    async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Builds a router with a list of pubkeys and required channels.
    #[method(name = "build_router")]
    async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned>;

    /// Sends a payment to a peer with specified router
    /// This method differs from SendPayment in that it allows users to specify a full route manually.
    /// This can be used for things like rebalancing.
    #[method(name = "send_payment_with_router")]
    async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;
}

pub struct PaymentRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    _store: S,
}

impl<S> PaymentRpcServerImpl<S> {
    pub fn new(actor: ActorRef<NetworkActorMessage>, _store: S) -> Self {
        PaymentRpcServerImpl { actor, _store }
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> PaymentRpcServer for PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    /// Sends a payment to a peer.
    async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.send_payment(params).await
    }

    /// Retrieves a payment.
    async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.get_payment(params).await
    }

    /// Builds a router with a list of pubkeys and required channels.
    async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned> {
        self.build_router(params).await
    }

    /// Sends a payment to a peer with specified router
    /// This method differs from SendPayment in that it allows users to specify a full route manually.
    /// This can be used for things like rebalancing.
    async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.send_payment_with_router(params).await
    }
}

impl<S> PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    pub async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey: params.target_pubkey,
                    amount: params.amount,
                    payment_hash: params.payment_hash,
                    final_tlc_expiry_delta: params.final_tlc_expiry_delta,
                    tlc_expiry_limit: params.tlc_expiry_limit,
                    invoice: params.invoice.clone(),
                    timeout: params.timeout,
                    max_fee_amount: params.max_fee_amount,
                    max_fee_rate: params.max_fee_rate,
                    max_parts: params.max_parts,
                    trampoline_hops: params.trampoline_hops.clone(),
                    keysend: params.keysend,
                    udt_type_script: params.udt_type_script.clone().map(|s| s.into()),
                    allow_self_payment: params.allow_self_payment.unwrap_or(false),
                    custom_records: params.custom_records.clone().map(|records| records.into()),
                    hop_hints: params
                        .hop_hints
                        .clone()
                        .map(|hints| hints.into_iter().map(|hint| hint.into()).collect()),
                    dry_run: params.dry_run.unwrap_or(false),
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params).map(|response| GetPaymentCommandResult {
            payment_hash: response.payment_hash,
            status: response.status,
            created_at: response.created_at,
            last_updated_at: response.last_updated_at,
            failed_error: response.failed_error,
            fee: response.fee,
            custom_records: response
                .custom_records
                .map(|records| PaymentCustomRecords { data: records.data }),
            #[cfg(debug_assertions)]
            routers: response.routers.clone(),
        })
    }

    pub async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(
                params.payment_hash,
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params).map(|response| GetPaymentCommandResult {
            payment_hash: response.payment_hash,
            status: response.status,
            last_updated_at: response.last_updated_at,
            created_at: response.created_at,
            failed_error: response.failed_error,
            fee: response.fee,
            custom_records: response
                .custom_records
                .map(|records| PaymentCustomRecords { data: records.data }),
            #[cfg(debug_assertions)]
            routers: response.routers.clone(),
        })
    }

    pub async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::BuildPaymentRouter(
                BuildRouterCommand {
                    amount: params.amount,
                    hops_info: params.hops_info.clone(),
                    udt_type_script: params.udt_type_script.clone().map(|x| x.into()),
                    final_tlc_expiry_delta: params.final_tlc_expiry_delta,
                },
                rpc_reply,
            ))
        };

        handle_actor_call!(self.actor, message, params).map(|response| BuildPaymentRouterResult {
            router_hops: response.router_hops,
        })
    }

    pub async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    payment_hash: params.payment_hash,
                    router: params.router.clone(),
                    invoice: params.invoice.clone(),
                    keysend: params.keysend,
                    udt_type_script: params.udt_type_script.clone().map(|s| s.into()),
                    custom_records: params.custom_records.clone().map(|records| records.into()),
                    dry_run: params.dry_run.unwrap_or(false),
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params).map(|response| GetPaymentCommandResult {
            payment_hash: response.payment_hash,
            status: response.status,
            created_at: response.created_at,
            last_updated_at: response.last_updated_at,
            failed_error: response.failed_error,
            fee: response.fee,
            custom_records: response
                .custom_records
                .map(|records| PaymentCustomRecords { data: records.data }),
            #[cfg(debug_assertions)]
            routers: response.routers.clone(),
        })
    }
}
