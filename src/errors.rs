use ractor::{MessagingErr, SpawnErr};
use tentacle::{error::SendErrorKind, secio::PeerId};
use thiserror::Error;

use crate::{
    ckb::FundingError,
    fiber::{
        channel::{ChannelActorMessage, ProcessingChannelError},
        graph::PathFindError,
        types::Hash256,
        NetworkActorMessage,
    },
};

use crate::invoice::InvoiceError;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Peer not found error: {0:?}")]
    PeerNotFound(PeerId),
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
    #[error("Failed to processing channel: {0}")]
    ChannelError(#[from] ProcessingChannelError),
    #[error("Invoice error: {0:?}")]
    CkbInvoiceError(#[from] InvoiceError),
    #[error("Funding error: {0}")]
    FundingError(#[from] FundingError),
    #[error("Send payment error: {0}")]
    SendPaymentError(String),
    #[error("Send payment first hop error: {0}")]
    SendPaymentFirstHopError(String),
    #[error("InvalidParameter: {0}")]
    InvalidParameter(String),
    #[error("Network Graph error: {0}")]
    NetworkGraphError(#[from] PathFindError),
    #[error("Invalid peer message: {0}")]
    InvalidPeerMessage(String),
    #[error("Onion packet error: {0}")]
    InvalidOnionPacket(crate::fiber::types::Error),
    #[error("Database error: {0}")]
    DBInternalError(String),
}

pub type Result<T> = std::result::Result<T, Error>;
