use bitflags::bitflags;
use ckb_hash::new_blake2b;
use log::{debug, error, warn};

use std::fmt::Debug;

use ckb_types::packed::{Byte, Byte32, Byte32Builder, OutPoint, Transaction};
use molecule::prelude::Entity;
use tentacle::secio::PeerId;
use thiserror::Error;

use super::types::{ChannelReady, CommitmentSigned, Privkey, Pubkey};
use crate::ckb::types::{AcceptChannel, OpenChannel};
use molecule::prelude::Builder;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub state: ChannelState,
    pub temp_id: Byte32,
    pub total_value: u64,
    pub to_self_value: u64,
    pub signer: InMemorySigner,
    pub holder_pubkeys: ChannelPublicKeys,

    pub counterparty_pubkeys: Option<ChannelPublicKeys>,

    pub funding_tx: Option<OutPoint>,
}

fn blake2b_hash_with_salt(data: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(salt);
    hasher.update(data);
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {
    AcceptChannel(AcceptChannel),
    CommitmentSigned(CommitmentSigned),
    ChannelReady(ChannelReady),
}

pub type ProcessingChannelResult = Result<(), ProcessingChannelError>;

#[derive(Error, Debug)]
pub enum ProcessingChannelError {
    #[error("Invalid chain hash: {0}")]
    InvalidChainHash(Byte32),
    #[error("Unsupported operation: {0}")]
    Unsupported(String),
    #[error("Invalid state")]
    InvalidState,
    #[error("Invalid Parameter: {0}")]
    InvalidParameter(String),
}

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ChannelReadyFlags: u32 {
        /// Indicates that we have sent a `commitment_signed` but are awaiting the responding
        ///	`revoke_and_ack` message. During this period, we can't generate new messages as
        /// we'd be unable to determine which TLCs they included in their `revoke_and_ack`
        ///	implicit ACK, so instead we have to hold them away temporarily to be sent later.
        const AWAITING_REMOTE_REVOKE = 1;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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

fn new_channel_id(seed: &[u8]) -> Byte32 {
    Byte32Builder::default()
        .set(
            blake2b_hash_with_salt(seed, b"channel id".as_slice())
                .into_iter()
                .map(Byte::new)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )
        .build()
}

impl Channel {
    pub fn new_inbound_channel<'a>(
        id: Byte32,
        seed: &[u8],
        counterparty_peer_id: PeerId,
        counterparty_value: u64,
        counterparty_pubkeys: ChannelPublicKeys,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let holder_pubkeys = (&signer).into();

        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
            total_value: counterparty_value,
            temp_id: id,
            to_self_value: 0,
            signer,
            holder_pubkeys,
            counterparty_pubkeys: Some(counterparty_pubkeys),
        }
    }

    pub fn new_outbound_channel(seed: &[u8], value: u64, to_self_value: u64) -> Self {
        assert!(
            to_self_value <= value,
            "to_self_value must be less than or equal to value"
        );
        let channel_id = new_channel_id(seed);
        let signer = InMemorySigner::generate_from_seed(seed);
        let holder_pubkeys = signer.to_publickeys();
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
            total_value: value,
            temp_id: channel_id.into(),
            to_self_value,
            signer,
            holder_pubkeys,
            counterparty_pubkeys: None,
        }
    }

    pub fn handle_accept_channel(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if accept_channel.funding_amount != self.total_value {
            return Err(ProcessingChannelError::InvalidParameter(
                "funding_amount mismatch".to_string(),
            ));
        }
        self.state = ChannelState::FundingNegotiated;
        let counterparty_pubkeys = (&accept_channel).into();
        self.counterparty_pubkeys = Some(counterparty_pubkeys);

        debug!("OpenChannel: {:?}", &accept_channel);
        Ok(())
    }

    pub fn handle_commitment_signed(
        &mut self,
        commitment_signed: CommitmentSigned,
    ) -> ProcessingChannelResult {
        debug!("CommitmentSigned: {:?}", &commitment_signed);
        Ok(())
    }

    pub fn step(&mut self, event: ChannelEvent) -> ProcessingChannelResult {
        match event {
            ChannelEvent::AcceptChannel(accept_channel) => {
                self.handle_accept_channel(accept_channel)
            }
            ChannelEvent::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed(commitment_signed)
            }
            _ => {
                warn!("Unexpected event: {:?}", event);
                Ok(())
            }
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

/// The commitment transactions are the transaction that each parties holds to
/// spend the funding transaction and create a new partition of the channel
/// balance between each parties. This struct contains all the information
/// that we need to build the actual CKB transaction.
/// Note that these commitment transactions are asymmetrical,
/// meaning that counterparties have different resulting CKB transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitmentTransaction {
    // Will change after each commitment to derive new commitment secrets.
    // Currently always 0.
    pub commitment_number: u64,
    // The broadcaster's balance, may be spent after timelock by the broadcaster or
    // by the countersignatory with revocation key.
    pub to_broadcaster_value: u64,
    // The countersignatory's balance, may be spent immediately by the countersignatory.
    pub to_countersignatory_value: u64,
    // The list of TLC commitmentments. These outputs are already multisiged to another
    // set of transactions, whose output may be spent by countersignatory with revocation key,
    // the original sender after delay, or the receiver if they has correct preimage,
    pub tlcs: Vec<TLCOutputInCommitment>,
    // A cache of the parties' pubkeys required to construct the transaction.
    pub keys: TxCreationKeys,
}

/// A wrapper on CommitmentTransaction indicating that the derived fields (the built bitcoin
/// transaction and the transaction creation keys) are trusted.
///
/// See trust() and verify() functions on CommitmentTransaction.
///
/// This structure implements Deref.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TrustedCommitmentTransaction<'a> {
    inner: &'a CommitmentTransaction,
}

impl CommitmentTransaction {
    pub fn build_tx(&self) -> Transaction {
        unimplemented!("build_tx");
    }

    fn internal_rebuild_transaction(
        &self,
        keys: &TxCreationKeys,
        channel_parameters: &DirectedChannelTransactionParameters,
        broadcaster_funding_pubkey: &Pubkey,
        countersignatory_funding_pubkey: &Pubkey,
    ) -> Result<Transaction, ()> {
        unimplemented!("internal_rebuild_transaction");
    }

    /// Trust our pre-built transaction and derived transaction creation public keys.
    ///
    /// Applies a wrapper which allows access to these fields.
    ///
    /// This should only be used if you fully trust the builder of this object.  It should not
    /// be used by an external signer - instead use the verify function.
    pub fn trust(&self) -> TrustedCommitmentTransaction {
        TrustedCommitmentTransaction { inner: self }
    }

    /// Verify our pre-built transaction and derived transaction creation public keys.
    ///
    /// Applies a wrapper which allows access to these fields.
    ///
    /// An external validating signer must call this method before signing
    /// or using the built transaction.
    pub fn verify(
        &self,
        channel_parameters: &DirectedChannelTransactionParameters,
        broadcaster_keys: &ChannelPublicKeys,
        countersignatory_keys: &ChannelPublicKeys,
    ) -> Result<TrustedCommitmentTransaction, ()> {
        // This is the only field of the key cache that we trust
        let per_commitment_point = &self.keys.per_commitment_point;
        let keys = TxCreationKeys::from_channel_static_keys(
            per_commitment_point,
            broadcaster_keys,
            countersignatory_keys,
        );
        if keys != self.keys {
            return Err(());
        }
        let tx = self.internal_rebuild_transaction(
            &keys,
            channel_parameters,
            &broadcaster_keys.funding_pubkey,
            &countersignatory_keys.funding_pubkey,
        )?;
        if self.build_tx().as_bytes() != tx.as_bytes() {
            return Err(());
        }
        Ok(TrustedCommitmentTransaction { inner: self })
    }
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

/// Static channel fields used to build transactions given per-commitment fields, organized by
/// broadcaster/countersignatory.
///
/// This is derived from the holder/counterparty-organized ChannelTransactionParameters via the
/// as_holder_broadcastable and as_counterparty_broadcastable functions.
pub struct DirectedChannelTransactionParameters<'a> {
    /// The holder's channel static parameters
    inner: &'a FundedChannelParameters,
    /// Whether the holder is the broadcaster
    holder_is_broadcaster: bool,
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl From<&OpenChannel> for ChannelPublicKeys {
    fn from(value: &OpenChannel) -> Self {
        ChannelPublicKeys {
            funding_pubkey: value.funding_pubkey.clone(),
            revocation_basepoint: value.revocation_basepoint.clone(),
            payment_point: value.payment_basepoint.clone(),
            delayed_payment_basepoint: value.delayed_payment_basepoint.clone(),
            tlc_basepoint: value.tlc_basepoint.clone(),
        }
    }
}

impl From<&AcceptChannel> for ChannelPublicKeys {
    fn from(value: &AcceptChannel) -> Self {
        ChannelPublicKeys {
            funding_pubkey: value.funding_pubkey.clone(),
            revocation_basepoint: value.revocation_basepoint.clone(),
            payment_point: value.payment_basepoint.clone(),
            delayed_payment_basepoint: value.delayed_payment_basepoint.clone(),
            tlc_basepoint: value.tlc_basepoint.clone(),
        }
    }
}

pub struct CounterpartyChannelTransactionParameters {
    /// Counter-party public keys
    pub pubkeys: ChannelPublicKeys,
    /// The contest delay selected by the counterparty, which applies to holder-broadcast transactions
    pub selected_contest_delay: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(PartialEq, Eq, Clone, Debug)]
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

pub fn derive_public_key(basepoint: &Pubkey, per_commitment_point: &Pubkey) -> Pubkey {
    // TODO: Currently we only copy the input pubkey. We need to actually derive new public keys
    // from the per_commitment_point.
    basepoint.clone()
}

/// A simple implementation of [`WriteableEcdsaChannelSigner`] that just keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InMemorySigner {
    /// Holder secret key in the 2-of-2 multisig script of a channel. This key also backs the
    /// holder's anchor output in a commitment transaction, if one is present.
    pub funding_key: Privkey,
    /// Holder secret key for blinded revocation pubkey.
    pub revocation_base_key: Privkey,
    /// Holder secret key used for our balance in counterparty-broadcasted commitment transactions.
    pub payment_key: Privkey,
    /// Holder secret key used in an HTLC transaction.
    pub delayed_payment_base_key: Privkey,
    /// Holder HTLC secret key used in commitment transaction HTLC outputs.
    pub htlc_base_key: Privkey,
    /// Commitment seed.
    pub commitment_seed: [u8; 32],
    /// Key derivation parameters.
    channel_keys_id: [u8; 32],
}

impl InMemorySigner {
    pub fn new(
        funding_key: Privkey,
        revocation_base_key: Privkey,
        payment_key: Privkey,
        delayed_payment_base_key: Privkey,
        htlc_base_key: Privkey,
        commitment_seed: [u8; 32],
        channel_keys_id: [u8; 32],
    ) -> Self {
        InMemorySigner {
            funding_key,
            revocation_base_key,
            payment_key,
            delayed_payment_base_key,
            htlc_base_key,
            commitment_seed,
            channel_keys_id,
        }
    }

    pub fn generate_from_seed(params: &[u8]) -> InMemorySigner {
        let seed = ckb_hash::blake2b_256(params);

        let commitment_seed = {
            let mut hasher = new_blake2b();
            hasher.update(&seed);
            hasher.update(&b"commitment seed"[..]);
            let mut result = [0u8; 32];
            hasher.finalize(&mut result);
            result
        };

        let key_derive = |seed: &[u8], info: &[u8]| {
            let mut hasher = new_blake2b();
            hasher.update(&seed);
            hasher.update(&b"commitment seed"[..]);
            let mut result = [0u8; 32];
            hasher.finalize(&mut result);
            Privkey::from_slice(&result)
        };

        let funding_key = key_derive(&seed, b"funding key");
        let revocation_base_key = key_derive(&seed, b"revocation base key");
        let payment_key = key_derive(&seed, b"payment key");
        let delayed_payment_base_key = key_derive(&seed, b"delayed payment base key");
        let htlc_base_key = key_derive(&seed, b"HTLC base key");

        Self::new(
            funding_key,
            revocation_base_key,
            payment_key,
            delayed_payment_base_key,
            htlc_base_key,
            commitment_seed,
            seed.clone(),
        )
    }

    fn to_publickeys(&self) -> ChannelPublicKeys {
        ChannelPublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            revocation_basepoint: self.revocation_base_key.pubkey(),
            payment_point: self.payment_key.pubkey(),
            delayed_payment_basepoint: self.delayed_payment_base_key.pubkey(),
            tlc_basepoint: self.htlc_base_key.pubkey(),
        }
    }
}

impl From<&InMemorySigner> for ChannelPublicKeys {
    fn from(signer: &InMemorySigner) -> Self {
        signer.to_publickeys()
    }
}

impl TxCreationKeys {
    /// Create per-state keys from channel base points and the per-commitment point.
    /// Key set is asymmetric and can't be used as part of counter-signatory set of transactions.
    pub fn derive_new(
        per_commitment_point: &Pubkey,
        broadcaster_delayed_payment_base: &Pubkey,
        broadcaster_tlc_base: &Pubkey,
        countersignatory_revocation_base: &Pubkey,
        countersignatory_tlc_base: &Pubkey,
    ) -> TxCreationKeys {
        TxCreationKeys {
            per_commitment_point: per_commitment_point.clone(),
            revocation_key: derive_public_key(
                countersignatory_revocation_base,
                per_commitment_point,
            ),
            broadcaster_htlc_key: derive_public_key(broadcaster_tlc_base, per_commitment_point),
            countersignatory_htlc_key: derive_public_key(
                countersignatory_tlc_base,
                per_commitment_point,
            ),
            broadcaster_delayed_payment_key: derive_public_key(
                broadcaster_delayed_payment_base,
                per_commitment_point,
            ),
        }
    }

    /// Generate per-state keys from channel static keys.
    /// Key set is asymmetric and can't be used as part of counter-signatory set of transactions.
    pub fn from_channel_static_keys(
        per_commitment_point: &Pubkey,
        broadcaster_keys: &ChannelPublicKeys,
        countersignatory_keys: &ChannelPublicKeys,
    ) -> TxCreationKeys {
        TxCreationKeys::derive_new(
            &per_commitment_point,
            &broadcaster_keys.delayed_payment_basepoint,
            &broadcaster_keys.tlc_basepoint,
            &countersignatory_keys.revocation_basepoint,
            &countersignatory_keys.tlc_basepoint,
        )
    }
}
