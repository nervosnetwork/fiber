use crate::ckb::{
    channel::{
        AddTlcCommand, ChannelCommand, ChannelCommandWithId, RemoveTlcCommand, ShutdownCommand,
    },
    network::{AcceptChannelCommand, OpenChannelCommand},
    types::{Hash256, LockTime, RemoveTlcFail, RemoveTlcFulfill},
    NetworkActorCommand, NetworkActorMessage,
};
use ckb_jsonrpc_types::Script;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::secio::PeerId;

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct OpenChannelParams {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub funding_amount: u128,
}

#[derive(Clone, Serialize)]
pub struct OpenChannelResult {
    pub temporary_channel_id: Hash256,
}

#[derive(Serialize, Deserialize)]
pub struct AcceptChannelParams {
    pub temporary_channel_id: Hash256,
    pub funding_amount: u128,
}

#[derive(Clone, Serialize)]
pub struct AcceptChannelResult {
    pub channel_id: Hash256,
}

// TODO @quake remove this unnecessary struct and rpc after refactoring
#[derive(Serialize, Deserialize)]
pub struct CommitmentSignedParams {
    pub channel_id: Hash256,
}

#[derive(Serialize, Deserialize)]
pub struct AddTlcParams {
    pub channel_id: Hash256,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: LockTime,
}

#[derive(Clone, Serialize)]
pub struct AddTlcResult {
    pub tlc_id: u64,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveTlcParams {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub reason: RemoveTlcReason,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill { payment_preimage: Hash256 },
    RemoveTlcFail { error_code: u32 },
}

#[derive(Serialize, Deserialize)]
pub struct ShutdownChannelParams {
    pub channel_id: Hash256,
    pub close_script: Script,
    pub fee: u128,
}

#[rpc(server)]
pub trait ChannelRpc {
    #[method(name = "open_channel")]
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned>;

    #[method(name = "accept_channel")]
    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned>;

    #[method(name = "commitment_signed")]
    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned>;

    #[method(name = "add_tlc")]
    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned>;

    #[method(name = "remove_tlc")]
    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned>;

    #[method(name = "shutdown_channel")]
    async fn shutdown_channel(&self, params: ShutdownChannelParams)
        -> Result<(), ErrorObjectOwned>;
}

pub struct ChannelRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
}

impl ChannelRpcServerImpl {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        ChannelRpcServerImpl { actor }
    }
}

#[async_trait]
impl ChannelRpcServer for ChannelRpcServerImpl {
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let message = |r| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: params.peer_id.clone(),
                    funding_amount: params.funding_amount.clone(),
                },
                Some(r),
            ))
        };
        match call!(self.actor, message).unwrap() {
            Ok(response) => Ok(OpenChannelResult {
                temporary_channel_id: response.channel_id,
            }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        let message = |r| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: params.temporary_channel_id,
                    funding_amount: params.funding_amount,
                },
                Some(r),
            ))
        };
        match call!(self.actor, message).unwrap() {
            Ok(response) => Ok(AcceptChannelResult {
                channel_id: response.new_channel_id,
            }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlPcnChannel(
            ChannelCommandWithId {
                channel_id: params.channel_id,
                command: ChannelCommand::CommitmentSigned(),
            },
        ));
        self.actor.cast(message).unwrap();
        Ok(())
    }

    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        let message = |r| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlPcnChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: params.amount,
                            preimage: None,
                            payment_hash: Some(params.payment_hash),
                            expiry: params.expiry,
                        },
                        Some(r),
                    ),
                },
            ))
        };
        match call!(self.actor, message).unwrap() {
            Ok(response) => Ok(AddTlcResult {
                tlc_id: response.tlc_id,
            }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlPcnChannel(
            ChannelCommandWithId {
                channel_id: params.channel_id,
                command: ChannelCommand::RemoveTlc(RemoveTlcCommand {
                    id: params.tlc_id,
                    reason: match params.reason {
                        RemoveTlcReason::RemoveTlcFulfill { payment_preimage } => {
                            crate::ckb::types::RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                                payment_preimage,
                            })
                        }
                        RemoveTlcReason::RemoveTlcFail { error_code } => {
                            crate::ckb::types::RemoveTlcReason::RemoveTlcFail(RemoveTlcFail {
                                error_code,
                            })
                        }
                    },
                }),
            },
        ));
        self.actor.cast(message).unwrap();
        Ok(())
    }

    async fn shutdown_channel(
        &self,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlPcnChannel(
            ChannelCommandWithId {
                channel_id: params.channel_id,
                command: ChannelCommand::Shutdown(ShutdownCommand {
                    close_script: params.close_script.into(),
                    fee: params.fee,
                }),
            },
        ));
        self.actor.cast(message).unwrap();
        Ok(())
    }
}
