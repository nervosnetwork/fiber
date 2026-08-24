use ckb_sdk::RpcError;
use ckb_types::packed::OutPoint;
use fiber_store::StoreError;
use ractor::{MessagingErr, SpawnErr};
use tentacle::error::SendErrorKind;
use thiserror::Error;

use crate::{
    ckb::FundingError,
    fiber::{
        channel::{ChannelActorMessage, ProcessingChannelError},
        graph::PathFindError,
        signer_actor::SignerActorMessage,
        InFlightCkbTxActorMessage, NetworkActorMessage,
    },
};
use fiber_types::{Hash256, Pubkey};
use tentacle::utils::TransportType;

use crate::invoice::InvoiceError;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Peer not found error: {0:?}")]
    PeerNotFound(Pubkey),
    #[error("No matching address for peer {0:?} with transport type {1:?}")]
    NoMatchingAddress(Pubkey, TransportType),
    #[error(
        "No supported address for peer {0:?} on this target; expected transports: {supported_transports}. Add a supported address or specify `addr_type` explicitly",
        supported_transports = if cfg!(target_arch = "wasm32") {
            "ws/wss"
        } else {
            "tcp"
        }
    )]
    NoSupportedAddress(Pubkey),
    #[error("Channel not found error: {0:?}")]
    ChannelNotFound(Hash256),
    #[error("Failed to send tentacle message: {0}")]
    TentacleSend(#[from] SendErrorKind),
    #[error("Failed to spawn actor: {0}")]
    SpawnErr(#[from] SpawnErr),
    #[error("Failed to send channel actor message: {0}")]
    ChannelMessagingErr(#[from] MessagingErr<ChannelActorMessage>),
    #[error("Failed to send network actor message: {0}")]
    NetworkMessagingErr(#[from] MessagingErr<NetworkActorMessage>),
    #[error("Fiber actor messaging error: {0}")]
    FiberMessagingErr(#[from] MessagingErr<crate::fiber::FiberActorMessage>),
    #[error("Failed to in-flight tx actor message: {0}")]
    InFlightCkbTxActorMessagingErr(#[from] MessagingErr<InFlightCkbTxActorMessage>),
    #[error("Failed to send signer actor message: {0}")]
    SignerMessagingErr(#[from] MessagingErr<SignerActorMessage>),
    #[error("Failed to processing channel: {0}")]
    ChannelError(#[from] ProcessingChannelError),
    #[error("Invoice error: {0:?}")]
    CkbInvoiceError(#[from] InvoiceError),
    #[error("Funding error: {0}")]
    FundingError(#[from] FundingError),
    #[error("Build payment route error: {0}")]
    BuildPaymentRouteError(String),
    #[error("Send payment error: {0}")]
    SendPaymentError(String),
    #[error("Send payment first hop error: {0}")]
    FirstHopError(String, bool),
    #[error("InvalidParameter: {0}")]
    InvalidParameter(String),
    #[error("Channel announcement verification deferred for {0:?}: {1}")]
    DeferredChannelAnnouncementVerification(OutPoint, String),
    #[error("Network Graph error: {0}")]
    NetworkGraphError(#[from] PathFindError),
    #[error("Invalid peer message: {0}")]
    InvalidPeerMessage(String),
    #[error("Onion packet error: {0}")]
    InvalidOnionPacket(crate::fiber::types::Error),
    #[error("Ckb Rpc error: {0}")]
    CkbRpcError(RpcError),
    #[error("Database error: {0}")]
    DBInternalError(String),
    #[error("Internal error: {0}")]
    InternalError(anyhow::Error),
    #[error("Invalid chain hash: {0} (expecting {1})")]
    InvalidChainHash(Hash256, Hash256),
    #[error("Secret key file error: {0}")]
    SecretKeyFileError(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::DBInternalError(msg) => Error::DBInternalError(msg),
            StoreError::IOError(err) => Error::IO(err),
            StoreError::RestoreError(msg) => Error::DBInternalError(msg),
            StoreError::BackupError(msg) => Error::DBInternalError(msg),
        }
    }
}
