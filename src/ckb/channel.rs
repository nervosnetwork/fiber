use bitflags::bitflags;

use ckb_types::packed::{Byte32, OutPoint, Transaction};

use super::types::{PCNMessage, Pubkey};

#[derive(Debug)]
pub struct Channel {
    state: ChannelState,
    funding_tx: Option<OutPoint>,
}

pub struct ClosedChannel {}

pub enum ChannelEvent {
    Message(PCNMessage),
}

bitflags! {
    #[derive(Debug, PartialEq, Eq)]
    struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Debug, PartialEq, Eq)]
    struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ChannelReadyFlags: u32 {
        /// Indicates that we have sent a `commitment_signed` but are awaiting the responding
        ///	`revoke_and_ack` message. During this period, we can't generate new messages as
        /// we'd be unable to determine which TLCs they included in their `revoke_and_ack`
        ///	implicit ACK, so instead we have to hold them away temporarily to be sent later.
        const AWAITING_REMOTE_REVOKE = 1;
    }
}

#[derive(Debug)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We have sent `funding_created` and are awaiting a `funding_signed` to advance to
    /// `AwaitingChannelReady`. Note that this is nonsense for an inbound channel as we immediately generate
    /// `funding_signed` upon receipt of `funding_created`, so simply skip this state.
    FundingNegotiated,
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady(ChannelReadyFlags),
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    /// is about to drop us, but we store this anyway.
    ShutdownComplete,
}

impl Channel {
    pub fn new_inbound_channel() -> Self {
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
        }
    }

    pub fn new_outbound_channel() -> Self {
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
        }
    }

    pub fn step(&mut self, event: ChannelEvent) {
        match event {
            ChannelEvent::Message(msg) => match msg {
                PCNMessage::OpenChannel(open_channel) => {
                    dbg!(open_channel);
                }
                _ => {}
            },
        }
    }

    pub fn is_funded(&self) -> bool {
        match self.state {
            ChannelState::ChannelReady(_) => {
                assert!(self.funding_tx.is_some());
                true
            }
            _ => false,
        }
    }

    pub fn must_get_funding_transaction(&self) -> OutPoint {
        self.funding_tx.clone().expect("Channel is not funded")
    }

    pub fn buil_commitment_tx(&self) -> CommitmentTransaction {
        unimplemented!("build_commitment_tx");
    }

    pub fn get_input_cells(&self) {
        let _channel = self.must_get_funding_transaction();
    }
}

pub struct CommitmentTransaction {
    commitment_number: u64,
    to_broadcaster_value_sat: u64,
    to_countersignatory_value_sat: u64,
    to_broadcaster_delay: Option<u16>, // Added in 0.0.117
    htlcs: Vec<TLCOutputInCommitment>,
    // A cache of the parties' pubkeys required to construct the transaction, see doc for trust()
    keys: TxCreationKeys,
    // For access to the pre-built transaction, see doc for trust()
    pub tx: Transaction,
}

pub struct FundedChannelParameters {
    /// Holder public keys
    pub holder_pubkeys: Pubkey,
    /// The contest delay selected by the holder, which applies to counterparty-broadcast transactions
    pub holder_selected_contest_delay: u16,
    /// Whether the holder is the initiator of this channel.
    /// This is an input to the commitment number obscure factor computation.
    pub is_outbound_from_holder: bool,
    /// The late-bound counterparty channel transaction parameters.
    /// These parameters are populated at the point in the protocol where the counterparty provides them.
    pub counterparty_parameters: Option<CounterpartyChannelTransactionParameters>,
    /// The late-bound funding outpoint
    pub funding_outpoint: OutPoint,
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug)]
pub struct ChannelPublicKeys {
    /// The public key which is used to sign all commitment transactions, as it appears in the
    /// on-chain channel lock-in 2-of-2 multisig output.
    pub funding_pubkey: Pubkey,
    /// The base point which is used (with derive_public_revocation_key) to derive per-commitment
    /// revocation keys. This is combined with the per-commitment-secret generated by the
    /// counterparty to create a secret which the counterparty can reveal to revoke previous
    /// states.
    pub revocation_basepoint: Pubkey,
    /// The public key on which the non-broadcaster (ie the countersignatory) receives an immediately
    /// spendable primary channel balance on the broadcaster's commitment transaction. This key is
    /// static across every commitment transaction.
    pub payment_point: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment payment
    /// public key which receives non-HTLC-encumbered funds which are only available for spending
    /// after some delay (or can be claimed via the revocation path).
    pub delayed_payment_basepoint: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment public key
    /// which is used to encumber HTLC-in-flight outputs.
    pub tlc_basepoint: Pubkey,
}

pub struct CounterpartyChannelTransactionParameters {
    /// Counter-party public keys
    pub pubkeys: ChannelPublicKeys,
    /// The contest delay selected by the counterparty, which applies to holder-broadcast transactions
    pub selected_contest_delay: u16,
}

#[derive(Debug)]
pub struct TLCOutputInCommitment {
    /// Whether the HTLC was "offered" (ie outbound in relation to this commitment transaction).
    /// Note that this is not the same as whether it is ountbound *from us*. To determine that you
    /// need to compare this value to whether the commitment transaction in question is that of
    /// the counterparty or our own.
    pub offered: bool,
    /// The value, in msat, of the HTLC. The value as it appears in the commitment transaction is
    /// this divided by 1000.
    pub amount: u64,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: u32,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Byte32,
    /// The position within the commitment transactions' outputs. This may be None if the value is
    /// below the dust limit (in which case no output appears in the commitment transaction and the
    /// value is spent to additional transaction fees).
    pub transaction_output_index: Option<u32>,
}

/// The set of public keys which are used in the creation of one commitment transaction.
/// These are derived from the channel base keys and per-commitment data.
///
/// A broadcaster key is provided from potential broadcaster of the computed transaction.
/// A countersignatory key is coming from a protocol participant unable to broadcast the
/// transaction.
///
/// These keys are assumed to be good, either because the code derived them from
/// channel basepoints via the new function, or they were obtained via
/// CommitmentTransaction.trust().keys() because we trusted the source of the
/// pre-calculated keys.
pub struct TxCreationKeys {
    /// The broadcaster's per-commitment public key which was used to derive the other keys.
    pub per_commitment_point: Pubkey,
    /// The revocation key which is used to allow the broadcaster of the commitment
    /// transaction to provide their counterparty the ability to punish them if they broadcast
    /// an old state.
    pub revocation_key: Pubkey,
    /// Broadcaster's HTLC Key
    pub broadcaster_htlc_key: Pubkey,
    /// Countersignatory's HTLC Key
    pub countersignatory_htlc_key: Pubkey,
    /// Broadcaster's Payment Key (which isn't allowed to be spent from for some delay)
    pub broadcaster_delayed_payment_key: Pubkey,
}
