#[cfg(debug_assertions)]
use crate::fiber::graph::SessionRouteNode as InternalSessionRouteNode;
use crate::fiber::serde_utils::SliceHex;
use crate::fiber::serde_utils::U32Hex;
use crate::fiber::{
    channel::ChannelActorStateStore,
    graph::PaymentSessionStatus,
    network::{HopHint as NetworkHopHint, SendPaymentCommand},
    serde_utils::{EntityHex, U128Hex, U64Hex},
    types::{Hash256, Pubkey},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
use ckb_types::packed::OutPoint;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use serde_with::serde_as;
use std::collections::HashMap;

use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};

/// RPC module for channel management.
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
}

pub(crate) struct PaymentRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    _store: S,
}

impl<S> PaymentRpcServerImpl<S> {
    pub(crate) fn new(actor: ActorRef<NetworkActorMessage>, _store: S) -> Self {
        PaymentRpcServerImpl { actor, _store }
    }
}

#[async_trait]
impl<S> PaymentRpcServer for PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    async fn send_payment(
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
                    max_parts: params.max_parts,
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
            router: response.router.nodes.into_iter().map(Into::into).collect(),
        })
    }

    async fn get_payment(
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
            router: response.router.nodes.into_iter().map(Into::into).collect(),
        })
    }
}
