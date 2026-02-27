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

/// The status of an outbound tlc
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum OutboundTlcStatus {
    // Offered tlc created and sent to remote party
    LocalAnnounced,
    // Received ACK from remote party for this offered tlc
    Committed,
    // Remote party removed this tlc
    RemoteRemoved,
    // We received another RemoveTlc message from peer when we are waiting for the ack of the last one.
    // So we need another ACK to confirm the removal.
    RemoveWaitPrevAck,
    // We have sent commitment signed to peer and waiting ACK for confirming this RemoveTlc
    RemoveWaitAck,
    // We have received the ACK for the RemoveTlc, it's safe to remove this tlc
    RemoveAckConfirmed,
}

/// The status of an inbound tlc
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum InboundTlcStatus {
    // Received tlc from remote party, but not committed yet
    RemoteAnnounced,
    // We received another AddTlc peer message when we are waiting for the ack of the last one.
    // So we need another ACK to confirm the addition.
    AnnounceWaitPrevAck,
    // We have sent commitment signed to peer and waiting ACK for confirming this AddTlc
    AnnounceWaitAck,
    // We have received ACK from peer and Committed this tlc
    Committed,
    // We have removed this tlc, but haven't received ACK from peer
    LocalRemoved,
    // We have received the ACK for the RemoveTlc, it's safe to remove this tlc
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

// ============================================================
// CommitmentNumbers
// ============================================================

/// The initial commitment number for a channel.
pub const INITIAL_COMMITMENT_NUMBER: u64 = 0;

/// Tracks the local and remote commitment numbers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentNumbers {
    pub local: u64,
    pub remote: u64,
}

impl Default for CommitmentNumbers {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitmentNumbers {
    pub fn new() -> Self {
        Self {
            local: INITIAL_COMMITMENT_NUMBER,
            remote: INITIAL_COMMITMENT_NUMBER,
        }
    }

    pub fn get_local(&self) -> u64 {
        self.local
    }

    pub fn get_remote(&self) -> u64 {
        self.remote
    }

    pub fn increment_local(&mut self) {
        self.local += 1;
    }

    pub fn increment_remote(&mut self) {
        self.remote += 1;
    }
}

// ============================================================
// ChannelConstraints
// ============================================================

/// Channel constraints for TLC value and number limits.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct ChannelConstraints {
    /// The maximum value that can be in pending TLCs.
    pub max_tlc_value_in_flight: u128,
    /// The maximum number of TLCs that can be accepted.
    pub max_tlc_number_in_flight: u64,
}

impl ChannelConstraints {
    pub fn new(max_tlc_value_in_flight: u128, max_tlc_number_in_flight: u64) -> Self {
        Self {
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        }
    }
}

// ============================================================
// ChannelTlcInfo
// ============================================================

/// TLC-related information for a channel.
/// We can update this information through the channel update message.
#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelTlcInfo {
    /// The timestamp when the following information is updated.
    pub timestamp: u64,

    /// Whether this channel is enabled for TLC forwarding or not.
    pub enabled: bool,

    /// The fee rate for TLC transfers. We only have these values set when
    /// this is a public channel. Both sides may set this value differently.
    /// This is a fee that is paid by the sender of the TLC.
    /// The detailed calculation for the fee of forwarding TLCs is
    /// `fee = round_above(tlc_fee_proportional_millionths * tlc_value / 1,000,000)`.
    pub tlc_fee_proportional_millionths: u128,

    /// The expiry delta timestamp, in milliseconds, for the TLC.
    pub tlc_expiry_delta: u64,

    /// The minimal TLC value we can receive in relay TLC.
    pub tlc_minimum_value: u128,
}

impl ChannelTlcInfo {
    /// Create a new `ChannelTlcInfo` with the given parameters.
    pub fn new(
        tlc_minimum_value: u128,
        tlc_expiry_delta: u64,
        tlc_fee_proportional_millionths: u128,
        timestamp: u64,
    ) -> Self {
        Self {
            tlc_minimum_value,
            tlc_expiry_delta,
            tlc_fee_proportional_millionths,
            enabled: true,
            timestamp,
        }
    }
}

// ============================================================
// ChannelOpeningStatus
// ============================================================

/// The status of a channel opening operation initiated by the local node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelOpeningStatus {
    /// The `open_channel` RPC has been submitted and the `OpenChannel` message has been sent
    /// to the peer. We are waiting for the peer to respond with an `AcceptChannel` message.
    WaitingForPeer,
    /// The peer accepted the channel. We are now collaborating on the funding transaction.
    FundingTxBuilding,
    /// The funding transaction has been submitted to the chain and is awaiting confirmation.
    FundingTxBroadcasted,
    /// The funding transaction has been confirmed and the channel is fully open.
    ChannelReady,
    /// The channel opening failed. The `failure_detail` field contains the reason.
    Failed,
}

// ============================================================
// ChannelBasePublicKeys
// ============================================================

use crate::Pubkey;

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBasePublicKeys {
    /// The public key which is used to sign all commitment transactions, as it appears in the
    /// on-chain channel lock-in 2-of-2 multisig output.
    pub funding_pubkey: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment public key
    /// which is used to encumber HTLC-in-flight outputs.
    pub tlc_base_key: Pubkey,
}

// ============================================================
// PrevTlcInfo
// ============================================================

use crate::Hash256;

/// When we are forwarding a TLC, we need to know the previous TLC information.
/// This struct keeps the information of the previous TLC.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct PrevTlcInfo {
    pub prev_channel_id: Hash256,
    /// The TLC is always a received TLC because we are forwarding it.
    pub prev_tlc_id: u64,
    pub forwarding_fee: u128,
    pub shared_secret: Option<[u8; 32]>,
}

impl PrevTlcInfo {
    pub fn new(prev_channel_id: Hash256, prev_tlc_id: u64, forwarding_fee: u128) -> Self {
        Self {
            prev_channel_id,
            prev_tlc_id,
            forwarding_fee,
            shared_secret: None,
        }
    }

    pub fn new_with_shared_secret(
        prev_channel_id: Hash256,
        prev_tlc_id: u64,
        forwarding_fee: u128,
        shared_secret: [u8; 32],
    ) -> Self {
        Self {
            prev_channel_id,
            prev_tlc_id,
            forwarding_fee,
            shared_secret: Some(shared_secret),
        }
    }
}

// ============================================================
// TlcInfo
// ============================================================

use crate::invoice::HashAlgorithm;
use crate::payment::{PaymentOnionPacket, RemoveTlcReason};

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct TlcInfo {
    pub status: TlcStatus,
    pub tlc_id: TLCId,
    pub amount: u128,
    pub payment_hash: Hash256,
    /// bolt04 total amount of the payment, must exist if payment secret is set
    pub total_amount: Option<u128>,
    /// bolt04 payment secret, only exists for last hop in multi-path payment
    pub payment_secret: Option<Hash256>,
    /// The attempt id associate with the tlc, only on outbound tlc
    /// only exists for first hop in multi-path payment
    pub attempt_id: Option<u64>,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    // the onion packet for multi-hop payment
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    ///
    /// Save it to backward errors. Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    #[serde(default)]
    pub is_trampoline_hop: bool,
    pub created_at: CommitmentNumbers,
    pub removed_reason: Option<RemoveTlcReason>,

    /// Note: `forwarding_tlc` is used to track the tlc chain for a multi-tlc payment.
    ///
    /// For an outbound tlc, this field records the previous (upstream) tlc,
    /// so we can walk backward when removing tlcs.
    ///
    /// For an inbound tlc, this field records the next (downstream) tlc,
    /// so we can continue tracking the forwarding path.
    ///
    /// Example:
    ///
    ///   Node A ---------> Node B ------------> Node C ------------> Node D
    ///   tlc_1  ---------> tlc_1(in) ---------> tlc_2(in) ---------> tlc_3
    ///                     tlc_2(out)           tlc_3(out)
    ///                forwarding_tlc        forwarding_tlc
    ///
    ///   forwarding_tlc relations:
    ///
    ///   - Node B:
    ///     - inbound: tlc_1.forwarding_tlc = Some((channel_BC, tlc2_id))
    ///     - outbound: tlc_2.forwarding_tlc = Some((channel_AB, tlc1_id))
    ///
    ///   - Node C:
    ///     - inbound: tlc_2.forwarding_tlc = Some((channel_CD, tlc3_id))
    ///     - outbound: tlc_3.forwarding_tlc = Some((channel_BC, tlc2_id))
    ///
    pub forwarding_tlc: Option<(Hash256, u64)>,
    pub removed_confirmed_at: Option<u64>,
    pub applied_flags: AppliedFlags,
}

use std::fmt;

impl fmt::Debug for TlcInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlcInfo")
            .field("status", &self.status)
            .field("tlc_id", &self.tlc_id)
            .field("amount", &self.amount)
            .field("payment_hash", &self.payment_hash)
            .field("expiry", &self.expiry)
            .field("created_at", &self.created_at)
            .field("removed_reason", &self.removed_reason)
            .field("applied_flags", &self.applied_flags)
            .finish()
    }
}

impl TlcInfo {
    pub fn log(&self) -> String {
        format!(
            "id: {:?} status: {:?} amount: {:?} removed: {:?} hash: {:?} ",
            &self.tlc_id, self.status, self.amount, self.removed_reason, self.payment_hash,
        )
    }

    pub fn id(&self) -> u64 {
        self.tlc_id.into()
    }

    pub fn is_offered(&self) -> bool {
        self.tlc_id.is_offered()
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn get_commitment_numbers(&self) -> CommitmentNumbers {
        self.created_at
    }

    pub fn flip_mut(&mut self) {
        self.tlc_id.flip_mut();
    }

    pub fn outbound_status(&self) -> OutboundTlcStatus {
        self.status.as_outbound_status()
    }

    pub fn inbound_status(&self) -> InboundTlcStatus {
        self.status.as_inbound_status()
    }

    pub fn is_fail_remove_confirmed(&self) -> bool {
        use crate::payment::RemoveTlcReason;
        matches!(self.removed_reason, Some(RemoveTlcReason::RemoveTlcFail(_)))
            && matches!(
                self.status,
                TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
                    | TlcStatus::Outbound(OutboundTlcStatus::RemoveWaitAck)
                    | TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed)
            )
    }

    /// Get the value for the field `htlc_type` in commitment lock witness.
    /// - Lowest 1 bit: 0 if the tlc is offered by the remote party, 1 otherwise.
    /// - High 7 bits:
    ///     - 0: ckb hash
    ///     - 1: sha256
    pub fn get_htlc_type(&self) -> u8 {
        let offered_flag = if self.is_offered() { 0u8 } else { 1u8 };
        ((self.hash_algorithm as u8) << 1) + offered_flag
    }
}

// ============================================================
// PendingTlcs
// ============================================================

/// A collection of pending TLCs.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct PendingTlcs {
    pub tlcs: Vec<TlcInfo>,
    pub next_tlc_id: u64,
}

impl PendingTlcs {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut TlcInfo> {
        self.tlcs.iter_mut()
    }

    pub fn get_next_id(&self) -> u64 {
        self.next_tlc_id
    }

    pub fn increment_next_id(&mut self) {
        self.next_tlc_id += 1;
    }

    pub fn add_tlc(&mut self, tlc: TlcInfo) {
        self.tlcs.push(tlc);
    }
}

// ============================================================
// TlcState
// ============================================================

/// The state of all TLCs for a channel.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct TlcState {
    pub offered_tlcs: PendingTlcs,
    pub received_tlcs: PendingTlcs,
    pub waiting_ack: bool,
}

impl TlcState {
    pub fn info(&self) -> String {
        format!(
            "offer_tlcs: {:?} received_tlcs: {:?}",
            self.offered_tlcs.tlcs.len(),
            self.received_tlcs.tlcs.len(),
        )
    }

    #[cfg(debug_assertions)]
    pub fn debug(&self) {
        let format_tlc_list = |tlcs: &[TlcInfo]| -> String {
            if tlcs.is_empty() {
                "    <none>".to_string()
            } else {
                tlcs.iter()
                    .map(|tlc| format!("    {}", tlc.log()))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        let offered_str = format_tlc_list(&self.offered_tlcs.tlcs);
        let received_str = format_tlc_list(&self.received_tlcs.tlcs);

        if offered_str.contains("<none>") && received_str.contains("<none>") {
            tracing::info!("TlcState: <none>");
        } else {
            tracing::info!(
                "TlcState:\n  Offered:\n{}\n  Received:\n{}",
                offered_str,
                received_str
            );
        }
    }

    pub fn get_mut(&mut self, tlc_id: &TLCId) -> Option<&mut TlcInfo> {
        self.offered_tlcs
            .tlcs
            .iter_mut()
            .find(|tlc| tlc.tlc_id == *tlc_id)
            .or_else(|| {
                self.received_tlcs
                    .tlcs
                    .iter_mut()
                    .find(|tlc| tlc.tlc_id == *tlc_id)
            })
    }

    pub fn get(&self, tlc_id: &TLCId) -> Option<&TlcInfo> {
        if tlc_id.is_offered() {
            self.offered_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.tlc_id == *tlc_id)
        } else {
            self.received_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.tlc_id == *tlc_id)
        }
    }

    pub fn get_committed_received_tlcs(&self) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.received_tlcs.tlcs.iter().filter(|tlc| {
            debug_assert!(tlc.is_received());
            matches!(tlc.inbound_status(), InboundTlcStatus::Committed)
        })
    }

    pub fn get_expired_offered_tlcs(
        &self,
        expect_expiry: u64,
    ) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs.tlcs.iter().filter(move |tlc| {
            tlc.outbound_status() != OutboundTlcStatus::LocalAnnounced
                && tlc.removed_confirmed_at.is_none()
                && tlc.expiry < expect_expiry
        })
    }

    pub fn get_next_offering(&self) -> u64 {
        self.offered_tlcs.get_next_id()
    }

    pub fn get_next_received(&self) -> u64 {
        self.received_tlcs.get_next_id()
    }

    pub fn increment_offering(&mut self) {
        self.offered_tlcs.increment_next_id();
    }

    pub fn increment_received(&mut self) {
        self.received_tlcs.increment_next_id();
    }

    pub fn set_waiting_ack(&mut self, waiting_ack: bool) {
        self.waiting_ack = waiting_ack;
    }

    pub fn all_tlcs(&self) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs
            .tlcs
            .iter()
            .chain(self.received_tlcs.tlcs.iter())
    }

    pub fn apply_remove_tlc(&mut self, tlc_id: TLCId) {
        if tlc_id.is_offered() {
            self.offered_tlcs.tlcs.retain(|tlc| tlc.tlc_id != tlc_id);
        } else {
            self.received_tlcs.tlcs.retain(|tlc| tlc.tlc_id != tlc_id);
        }
    }

    pub fn add_offered_tlc(&mut self, tlc: TlcInfo) {
        self.offered_tlcs.add_tlc(tlc);
    }

    pub fn add_received_tlc(&mut self, tlc: TlcInfo) {
        self.received_tlcs.add_tlc(tlc);
    }

    pub fn set_received_tlc_removed(&mut self, tlc_id: u64, reason: RemoveTlcReason) -> Hash256 {
        let tlc = self.get_mut(&TLCId::Received(tlc_id)).expect("get tlc");
        assert_eq!(tlc.inbound_status(), InboundTlcStatus::Committed);
        tlc.removed_reason = Some(reason);
        tlc.status = TlcStatus::Inbound(InboundTlcStatus::LocalRemoved);
        tlc.payment_hash
    }

    pub fn set_offered_tlc_removed(&mut self, tlc_id: u64, reason: RemoveTlcReason) -> Hash256 {
        let tlc = self.get_mut(&TLCId::Offered(tlc_id)).expect("get tlc");
        assert_eq!(tlc.outbound_status(), OutboundTlcStatus::Committed);
        tlc.removed_reason = Some(reason);
        tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoteRemoved);
        tlc.payment_hash
    }

    pub fn commitment_signed_tlcs(&self, for_remote: bool) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs
            .tlcs
            .iter()
            .filter(move |tlc| match tlc.outbound_status() {
                OutboundTlcStatus::LocalAnnounced => for_remote,
                OutboundTlcStatus::Committed => true,
                OutboundTlcStatus::RemoteRemoved => for_remote,
                OutboundTlcStatus::RemoveWaitPrevAck => for_remote,
                OutboundTlcStatus::RemoveWaitAck => false,
                OutboundTlcStatus::RemoveAckConfirmed => false,
            })
            .chain(
                self.received_tlcs
                    .tlcs
                    .iter()
                    .filter(move |tlc| match tlc.inbound_status() {
                        InboundTlcStatus::RemoteAnnounced => !for_remote,
                        InboundTlcStatus::AnnounceWaitPrevAck => !for_remote,
                        InboundTlcStatus::AnnounceWaitAck => true,
                        InboundTlcStatus::Committed => true,
                        InboundTlcStatus::LocalRemoved => !for_remote,
                        InboundTlcStatus::RemoveAckConfirmed => false,
                    }),
            )
    }

    pub fn update_for_commitment_signed(&mut self) -> bool {
        for tlc in self.offered_tlcs.tlcs.iter_mut() {
            if tlc.outbound_status() == OutboundTlcStatus::RemoteRemoved {
                let status = if self.waiting_ack {
                    OutboundTlcStatus::RemoveWaitPrevAck
                } else {
                    OutboundTlcStatus::RemoveWaitAck
                };
                tlc.status = TlcStatus::Outbound(status);
            }
        }
        for tlc in self.received_tlcs.tlcs.iter_mut() {
            if tlc.inbound_status() == InboundTlcStatus::RemoteAnnounced {
                let status = if self.waiting_ack {
                    InboundTlcStatus::AnnounceWaitPrevAck
                } else {
                    InboundTlcStatus::AnnounceWaitAck
                };
                tlc.status = TlcStatus::Inbound(status)
            }
        }
        self.need_another_commitment_signed()
    }

    pub fn update_for_revoke_and_ack(&mut self, commitment_number: CommitmentNumbers) {
        for tlc in self.offered_tlcs.tlcs.iter_mut() {
            match tlc.outbound_status() {
                OutboundTlcStatus::LocalAnnounced => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::Committed);
                }
                OutboundTlcStatus::RemoveWaitPrevAck => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveWaitAck);
                }
                OutboundTlcStatus::RemoveWaitAck => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed);
                    tlc.removed_confirmed_at = Some(commitment_number.get_local());
                }
                _ => {}
            }
        }

        for tlc in self.received_tlcs.tlcs.iter_mut() {
            match tlc.inbound_status() {
                InboundTlcStatus::AnnounceWaitPrevAck => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck);
                }
                InboundTlcStatus::AnnounceWaitAck => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::Committed);
                }
                InboundTlcStatus::LocalRemoved => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed);
                    tlc.removed_confirmed_at = Some(commitment_number.get_remote());
                }
                _ => {}
            }
        }
    }

    pub fn need_another_commitment_signed(&self) -> bool {
        self.offered_tlcs.tlcs.iter().any(|tlc| {
            let status = tlc.outbound_status();
            matches!(
                status,
                OutboundTlcStatus::LocalAnnounced
                    | OutboundTlcStatus::RemoteRemoved
                    | OutboundTlcStatus::RemoveWaitPrevAck
                    | OutboundTlcStatus::RemoveWaitAck
            )
        }) || self.received_tlcs.tlcs.iter().any(|tlc| {
            let status = tlc.inbound_status();
            matches!(
                status,
                InboundTlcStatus::RemoteAnnounced
                    | InboundTlcStatus::AnnounceWaitPrevAck
                    | InboundTlcStatus::AnnounceWaitAck
            )
        })
    }
}

// ============================================================
// AddTlcCommand
// ============================================================

/// Command to add a new TLC to a channel.
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub payment_hash: Hash256,
    /// The attempt id associated with the TLC.
    pub attempt_id: Option<u64>,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    /// Onion packet for the next node.
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    /// Save it for outbound (offered) TLC to backward errors.
    /// Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    /// Whether this outbound TLC is the trampoline-boundary hop.
    pub is_trampoline_hop: bool,
    pub previous_tlc: Option<PrevTlcInfo>,
}

impl fmt::Debug for AddTlcCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddTlcCommand")
            .field("amount", &self.amount)
            .field("payment_hash", &self.payment_hash)
            .field("attempt_id", &self.attempt_id)
            .field("expiry", &self.expiry)
            .field("hash_algorithm", &self.hash_algorithm)
            .field("is_trampoline_hop", &self.is_trampoline_hop)
            .field("previous_tlc", &self.previous_tlc)
            .finish()
    }
}

// ============================================================
// RetryableTlcOperation
// ============================================================

/// A retryable TLC operation that may need to be replayed after reconnection.
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Debug, Hash)]
pub enum RetryableTlcOperation {
    RemoveTlc(TLCId, RemoveTlcReason),
    AddTlc(AddTlcCommand),
}

// ============================================================
// ShutdownInfo
// ============================================================

use crate::serde_utils::PartialSignatureAsBytes;
use crate::EntityHex;
use ckb_types::packed::Script;
use musig2::PartialSignature;
use serde_with::serde_as;

/// Information about a channel shutdown.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Debug)]
pub struct ShutdownInfo {
    #[serde_as(as = "EntityHex")]
    pub close_script: Script,
    pub fee_rate: u64,
    #[serde_as(as = "Option<PartialSignatureAsBytes>")]
    pub signature: Option<PartialSignature>,
}

// ============================================================
// RevokeAndAck
// ============================================================

use crate::serde_utils::PubNonceAsBytes;
use musig2::PubNonce;

/// Message to revoke the previous commitment and acknowledge the new one.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeAndAck {
    pub channel_id: Hash256,
    #[serde_as(as = "PartialSignatureAsBytes")]
    pub revocation_partial_signature: PartialSignature,
    pub next_per_commitment_point: Pubkey,
    #[serde_as(as = "PubNonceAsBytes")]
    pub next_revocation_nonce: PubNonce,
}

// ============================================================
// PublicChannelInfo
// ============================================================

use crate::protocol::{ChannelAnnouncement, ChannelUpdate, EcdsaSignature};

// This struct holds the channel information that are only relevant when the channel
// is public. The information includes signatures to the channel announcement message,
// our config for the channel that will be published to the network (via ChannelUpdate).
// For ChannelUpdate config, only information on our side are saved here because we have no
// control to the config on the counterparty side. And they will publish
// the config to the network via another ChannelUpdate message.
#[serde_as]
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct PublicChannelInfo {
    /// Channel announcement signatures, may be empty for private channel.
    #[serde_as(as = "Option<(_, PartialSignatureAsBytes)>")]
    pub local_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,
    #[serde_as(as = "Option<(_, PartialSignatureAsBytes)>")]
    pub remote_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_channel_announcement_nonce: Option<PubNonce>,
    pub channel_announcement: Option<ChannelAnnouncement>,
    pub channel_update: Option<ChannelUpdate>,
}

impl PublicChannelInfo {
    pub fn new() -> Self {
        Default::default()
    }
}

// ============================================================
// InMemorySigner
// ============================================================

use crate::Privkey;

/// A simple implementation of a channel signer that keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InMemorySigner {
    /// Holder secret key in the 2-of-2 multisig script of a channel.
    pub funding_key: Privkey,
    /// Holder HTLC secret key used in commitment transaction HTLC outputs.
    pub tlc_base_key: Privkey,
    /// SecNonce used to generate valid signature in musig.
    pub musig2_base_nonce: Privkey,
    /// Seed to derive above keys (per commitment).
    pub commitment_seed: [u8; 32],
}

impl fmt::Debug for InMemorySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemorySigner")
            .field("funding_key", &"[REDACTED]")
            .field("tlc_base_key", &"[REDACTED]")
            .field("musig2_base_nonce", &"[REDACTED]")
            .field("commitment_seed", &"[REDACTED]")
            .finish()
    }
}

// ============================================================
// ChannelOpenRecord
// ============================================================

use serde_with::DisplayFromStr;
use tentacle_secio::PeerId;

/// A record that tracks a channel-opening attempt — either outbound (initiated by us)
/// or inbound (initiated by a remote peer and pending local acceptance).
///
/// Outbound records are created when `open_channel` is called.
/// Inbound records are created when an `OpenChannel` message is received from a peer.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelOpenRecord {
    /// The channel ID. For outbound channels this is initially the temporary ID; it is
    /// updated to the final channel ID once the peer sends `AcceptChannel`. For inbound
    /// channels, the temp ID is replaced by the computed new ID when `accept_channel` is
    /// called.
    pub channel_id: Hash256,
    /// The remote peer involved in this channel-opening attempt.
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    /// Whether the local node is the accepting side (received the `OpenChannel` request).
    pub is_acceptor: bool,
    /// Current status of the opening process.
    pub status: ChannelOpeningStatus,
    /// The local node's funding amount for the channel.
    /// For outbound channels this is what the initiator contributes.
    /// For inbound channels this is set to the remote peer's funding amount.
    pub funding_amount: u128,
    /// Human-readable description of why the opening failed, set only when `status == Failed`.
    pub failure_detail: Option<String>,
    /// Timestamp (milliseconds since UNIX epoch) when the record was created.
    pub created_at: u64,
    /// Timestamp (milliseconds since UNIX epoch) of the last status update.
    pub last_updated_at: u64,
}

impl ChannelOpenRecord {
    /// Create a new outbound record in the `WaitingForPeer` state.
    pub fn new(channel_id: Hash256, peer_id: PeerId, funding_amount: u128) -> Self {
        let now = crate::now_timestamp_as_millis_u64();
        Self {
            channel_id,
            peer_id,
            is_acceptor: false,
            status: ChannelOpeningStatus::WaitingForPeer,
            funding_amount,
            failure_detail: None,
            created_at: now,
            last_updated_at: now,
        }
    }

    /// Create a new outbound record in the `WaitingForPeer` state.
    /// Used when `open_channel` is called.
    pub fn new_outbound(channel_id: Hash256, peer_id: PeerId, funding_amount: u128) -> Self {
        Self::new(channel_id, peer_id, funding_amount)
    }

    /// Create a new inbound record in the `WaitingForPeer` state.
    /// Used when a remote peer's `OpenChannel` request is queued for local acceptance.
    pub fn new_inbound(channel_id: Hash256, peer_id: PeerId, remote_funding_amount: u128) -> Self {
        let mut record = Self::new(channel_id, peer_id, remote_funding_amount);
        record.is_acceptor = true;
        record
    }

    /// Transition to a new status.
    pub fn update_status(&mut self, status: ChannelOpeningStatus) {
        self.status = status;
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
    }

    /// Transition to `Failed` and record the reason.
    pub fn fail(&mut self, reason: String) {
        self.status = ChannelOpeningStatus::Failed;
        self.failure_detail = Some(reason);
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
    }
}

// ============================================================
// PendingNotifySettleTlc
// ============================================================

/// A TLC that is pending notification for settlement.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PendingNotifySettleTlc {
    pub payment_hash: Hash256,
    pub tlc_id: u64,
    /// The expire time if the TLC should be held.
    pub hold_expire_at: Option<u64>,
}

// ============================================================
// ChannelActorStateCore
// ============================================================

use ckb_types::packed::Transaction;
use ckb_types::H256;
use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

/// The core serializable state of a channel actor.
///
/// This struct contains all the persistable fields of a channel.
/// Runtime-only fields (like actor references) are managed separately in fiber-lib.
#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct ChannelActorStateCore {
    pub state: ChannelState,
    /// The data below are only relevant if the channel is public.
    pub public_channel_info: Option<PublicChannelInfo>,

    pub local_tlc_info: ChannelTlcInfo,
    pub remote_tlc_info: Option<ChannelTlcInfo>,

    /// The local public key used to establish p2p network connection.
    pub local_pubkey: Pubkey,
    /// The remote public key used to establish p2p network connection.
    pub remote_pubkey: Pubkey,

    pub id: Hash256,
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_tx: Option<Transaction>,

    pub funding_tx_confirmed_at: Option<(H256, u32, u64)>,

    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,

    /// Is this channel initially inbound?
    /// An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,

    /// Is this channel one-way?
    /// Combines with is_acceptor to determine if the channel able to send payment to the counterparty or not.
    pub is_one_way: bool,

    /// The amount of CKB/UDT that we own in the channel.
    /// This value will only change after we have resolved a tlc.
    pub to_local_amount: u128,
    /// The amount of CKB/UDT that the remote owns in the channel.
    /// This value will only change after we have resolved a tlc.
    pub to_remote_amount: u128,

    /// These two amounts used to keep the minimal ckb amount for the two parties.
    /// TLC operations will not affect these two amounts, only used to keep the commitment transactions
    /// to be valid, so that any party can close the channel at any time.
    pub local_reserved_ckb_amount: u64,
    pub remote_reserved_ckb_amount: u64,

    /// The commitment fee rate is used to calculate the fee for the commitment transactions.
    /// The side who want to submit the commitment transaction will pay fee.
    pub commitment_fee_rate: u64,

    /// The delay time for the commitment transaction, this value is set by the initiator of the channel.
    /// It must be a relative EpochNumberWithFraction in u64 format.
    pub commitment_delay_epoch: u64,

    /// The fee rate used for funding transaction, the initiator may set it as `funding_fee_rate` option,
    /// if it's not set, DEFAULT_FEE_RATE will be used as default value, two sides will use the same fee rate.
    pub funding_fee_rate: u64,

    /// Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    /// Cached channel public keys for easier of access.
    pub local_channel_public_keys: ChannelBasePublicKeys,

    /// Commitment numbers that are used to derive keys.
    /// This value is guaranteed to be 0 when channel is just created.
    pub commitment_numbers: CommitmentNumbers,

    pub local_constraints: ChannelConstraints,
    pub remote_constraints: ChannelConstraints,

    /// All the TLC related information.
    pub tlc_state: TlcState,

    /// The retryable tlc operations that are waiting to be processed.
    pub retryable_tlc_operations: VecDeque<RetryableTlcOperation>,
    pub waiting_forward_tlc_tasks: HashMap<TLCId, [u8; 32]>,

    /// The remote lock script for close channel, setup during the channel establishment.
    #[serde_as(as = "Option<EntityHex>")]
    pub remote_shutdown_script: Option<Script>,
    /// The local lock script for close channel.
    #[serde_as(as = "EntityHex")]
    pub local_shutdown_script: Script,

    /// Basically the latest remote nonce sent by the peer with the CommitmentSigned message,
    /// but we will only update this field after we have sent a RevokeAndAck to the peer.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub last_committed_remote_nonce: Option<PubNonce>,

    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_revocation_nonce_for_verify: Option<PubNonce>,
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_revocation_nonce_for_send: Option<PubNonce>,
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_revocation_nonce_for_next: Option<PubNonce>,

    /// The latest commitment transaction we're holding,
    /// it can be broadcasted to blockchain by us to force close the channel.
    #[serde_as(as = "Option<EntityHex>")]
    pub latest_commitment_transaction: Option<Transaction>,

    /// All the commitment point that are sent from the counterparty.
    /// We need to save all these points to derive the keys for the commitment transactions.
    pub remote_commitment_points: Vec<(u64, Pubkey)>,
    pub remote_channel_public_keys: Option<ChannelBasePublicKeys>,

    /// The shutdown info for both local and remote, setup by the shutdown command or message.
    pub local_shutdown_info: Option<ShutdownInfo>,
    pub remote_shutdown_info: Option<ShutdownInfo>,

    /// Transaction hash of the shutdown transaction.
    /// The shutdown transaction can be COOPERATIVE or UNCOOPERATIVE.
    pub shutdown_transaction_hash: Option<H256>,

    /// A flag to indicate whether the channel is reestablishing,
    /// we won't process any messages until the channel is reestablished.
    pub reestablishing: bool,
    pub last_revoke_ack_msg: Option<RevokeAndAck>,

    pub created_at: SystemTime,
}

// ============================================================
// Molecule conversions
// ============================================================

use crate::gen::fiber as molecule_fiber;
use ckb_types::packed::Byte32 as MByte32;
use molecule::prelude::{Builder, Entity};
use musig2::BinaryEncoding;

fn partial_signature_to_molecule(partial_signature: PartialSignature) -> MByte32 {
    MByte32::from_slice(partial_signature.serialize().as_ref()).expect("[Byte; 32] from [u8; 32]")
}

fn pub_nonce_to_molecule(pub_nonce: PubNonce) -> molecule_fiber::PubNonce {
    molecule_fiber::PubNonce::from_slice(pub_nonce.to_bytes().as_ref())
        .expect("PubNonce from 66 bytes")
}

impl From<PubNonce> for molecule_fiber::PubNonce {
    fn from(value: PubNonce) -> Self {
        molecule_fiber::PubNonce::from_slice(value.to_bytes().as_ref())
            .expect("valid pubnonce serialized to 66 bytes")
    }
}

impl TryFrom<molecule_fiber::PubNonce> for PubNonce {
    type Error = musig2::errors::DecodeError<PubNonce>;

    fn try_from(value: molecule_fiber::PubNonce) -> Result<Self, Self::Error> {
        PubNonce::from_bytes(value.as_slice())
    }
}

impl From<RevokeAndAck> for molecule_fiber::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_fiber::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .revocation_partial_signature(partial_signature_to_molecule(
                revoke_and_ack.revocation_partial_signature,
            ))
            .next_per_commitment_point(revoke_and_ack.next_per_commitment_point.into())
            .next_revocation_nonce(pub_nonce_to_molecule(revoke_and_ack.next_revocation_nonce))
            .build()
    }
}

impl TryFrom<molecule_fiber::RevokeAndAck> for RevokeAndAck {
    type Error = anyhow::Error;

    fn try_from(revoke_and_ack: molecule_fiber::RevokeAndAck) -> Result<Self, Self::Error> {
        Ok(RevokeAndAck {
            channel_id: revoke_and_ack.channel_id().into(),
            revocation_partial_signature: PartialSignature::from_slice(
                revoke_and_ack.revocation_partial_signature().as_slice(),
            )
            .map_err(|e| anyhow::anyhow!(e))?,
            next_per_commitment_point: revoke_and_ack
                .next_per_commitment_point()
                .try_into()
                .map_err(|e: secp256k1::Error| anyhow::anyhow!(e))?,
            next_revocation_nonce: PubNonce::from_bytes(
                revoke_and_ack.next_revocation_nonce().as_slice(),
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?,
        })
    }
}
