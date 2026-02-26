//! Channel-related types: state flags, TLC status, channel state enum.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

// ============================================================
// Channel bitflags
// ============================================================

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelFlags: u8 {
        const PUBLIC = 1;
        const ONE_WAY = 1 << 1;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelUpdateChannelFlags: u32 {
        const DISABLED = 1;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelUpdateMessageFlags: u32 {
        const UPDATE_OF_NODE1 = 0;
        const UPDATE_OF_NODE2 = 1;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CollaboratingFundingTxFlags: u32 {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABORATION_COMPLETED = CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT.bits() | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct SigningCommitmentFlags: u32 {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT.bits() | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingTxSignaturesFlags: u32 {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT.bits() | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ShuttingDownFlags: u32 {
        const OUR_SHUTDOWN_SENT = 1;
        const THEIR_SHUTDOWN_SENT = 1 << 1;
        const AWAITING_PENDING_TLCS = ShuttingDownFlags::OUR_SHUTDOWN_SENT.bits() | ShuttingDownFlags::THEIR_SHUTDOWN_SENT.bits();
        const DROPPING_PENDING = 1 << 2;
        const WAITING_COMMITMENT_CONFIRMATION = 1 << 3;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CloseFlags: u32 {
        const COOPERATIVE = 1;
        const UNCOOPERATIVE_LOCAL = 1 << 1;
        const ABANDONED = 1 << 2;
        const FUNDING_ABORTED = 1 << 3;
        const UNCOOPERATIVE_REMOTE = 1 << 4;
        const WAITING_ONCHAIN_SETTLEMENT = 1 << 5;
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    #[serde(transparent)]
    pub struct AppliedFlags: u8 {
        const ADD = 1;
        const REMOVE = 1 << 1;
    }
}

// ============================================================
// TLC types
// ============================================================

/// The id of a TLC, it can be either offered or received.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
pub enum TLCId {
    /// Offered TLC id
    Offered(u64),
    /// Received TLC id
    Received(u64),
}

impl From<TLCId> for u64 {
    fn from(id: TLCId) -> u64 {
        match id {
            TLCId::Offered(id) => id,
            TLCId::Received(id) => id,
        }
    }
}

impl TLCId {
    pub fn is_offered(&self) -> bool {
        matches!(self, TLCId::Offered(_))
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn flip(&self) -> Self {
        match self {
            TLCId::Offered(id) => TLCId::Received(*id),
            TLCId::Received(id) => TLCId::Offered(*id),
        }
    }

    pub fn flip_mut(&mut self) {
        *self = self.flip();
    }
}

/// The status of an outbound TLC
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum OutboundTlcStatus {
    LocalAnnounced,
    Committed,
    RemoteRemoved,
    RemoveWaitPrevAck,
    RemoveWaitAck,
    RemoveAckConfirmed,
}

/// The status of an inbound TLC
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum InboundTlcStatus {
    RemoteAnnounced,
    AnnounceWaitPrevAck,
    AnnounceWaitAck,
    Committed,
    LocalRemoved,
    RemoveAckConfirmed,
}

/// The status of a TLC
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum TlcStatus {
    /// Outbound TLC
    Outbound(OutboundTlcStatus),
    /// Inbound TLC
    Inbound(InboundTlcStatus),
}

impl TlcStatus {
    pub fn as_outbound_status(&self) -> OutboundTlcStatus {
        match self {
            TlcStatus::Outbound(status) => status.clone(),
            _ => {
                unreachable!("unexpected status")
            }
        }
    }

    pub fn as_inbound_status(&self) -> InboundTlcStatus {
        match self {
            TlcStatus::Inbound(status) => status.clone(),
            _ => {
                unreachable!("unexpected status ")
            }
        }
    }
}

// ============================================================
// Channel state
// ============================================================

/// The state of a channel.
///
/// Note: fiber-lib uses default serde (bincode-compatible), while fiber-json-types
/// uses `#[serde(tag = "state_name", content = "state_flags")]` for JSON.
/// This definition uses the default (bincode-compatible) representation.
/// The JSON-specific tagged version is defined in fiber-json-types.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady,
    /// We've successfully negotiated a `closing_signed` dance.
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

impl ChannelState {
    pub fn is_closed(&self) -> bool {
        matches!(
            self,
            ChannelState::Closed(_)
                | ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
        )
    }

    pub fn can_abort_funding(&self) -> bool {
        match self {
            ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::SigningCommitment(_) => true,
            ChannelState::AwaitingTxSignatures(flags)
                if !flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT) =>
            {
                true
            }
            _ => false,
        }
    }
}

impl ShuttingDownFlags {
    pub fn is_ok_for_commitment_operation(&self) -> bool {
        !self.contains(ShuttingDownFlags::DROPPING_PENDING)
            && !self.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
    }
}
