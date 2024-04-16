use bitflags::bitflags;
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_types::packed::{OutPoint, Transaction};
use log::{debug, error, info};
use molecule::prelude::Entity;
use musig2::SecNonce;
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;

use std::fmt::Debug;

use super::{
    network::PCNMessageWithPeerId,
    peer::PeerActorMessage,
    types::{
        AcceptChannel, ChannelReady, CommitmentSigned, Hash256, OpenChannel, PCNMessage, Privkey,
        Pubkey,
    },
    NetworkActorCommand, NetworkActorMessage,
};

pub type ChannelActorMessage = PCNMessage;

#[derive(Clone, Debug, Deserialize)]
pub enum ChannelCommand {
    OpenChannel(OpenChannelCommand),
}

pub const HOLDER_INITIAL_COMMITMENT_NUMBER: u64 = 0;
pub const COUNTERPARTY_INITIAL_COMMITMENT_NUMBER: u64 = 2 ^ 48 - 1;
pub const DEFAULT_FEE_RATE: u64 = 0;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 0;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u64 = u64::MAX;
pub const DEFAULT_MAX_ACCEPT_TLCS: u64 = u64::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u64 = 0;
pub const DEFAULT_TO_SELF_DELAY: u64 = 10;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct OpenChannelCommand {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub total_value: u64,
    pub to_self_value: u64,
}

impl OpenChannelCommand {
    pub fn create_channel(&self) -> Result<ChannelActorState, ProcessingChannelError> {
        // Use a deterministic RNG for now to facilitate development.
        let seed = 42u64.to_le_bytes();

        Ok(ChannelActorState::new_outbound_channel(
            &seed,
            self.total_value,
            self.to_self_value,
            DEFAULT_TO_SELF_DELAY,
        ))
    }
}

pub enum ChannelInitializationParameter {
    /// To open a new channel to another peer, we process OpenChannelCommand
    /// and create a new outgoing channel.
    OpenChannelCommand(OpenChannelCommand),
    /// To accept a new channel from another peer, we process received
    /// OpenChannel message and create a incoming channel.
    OpenChannel(PeerId, usize, OpenChannel),
}

#[derive(Debug)]
pub struct ChannelActor {
    network: ActorRef<NetworkActorMessage>,
    peer: ActorRef<PeerActorMessage>,
}

impl ChannelActor {
    pub fn new(network: ActorRef<NetworkActorMessage>, peer: ActorRef<PeerActorMessage>) -> Self {
        Self { network, peer }
    }
}

#[rasync_trait]
impl Actor for ChannelActor {
    // An actor has a message type
    type Msg = PCNMessage;
    // and (optionally) internal state
    type State = ChannelActorState;
    // Startup arguments for actor initialization
    type Arguments = ChannelInitializationParameter;

    // Initially we need to create our state, and potentially
    // start some internal processing (by posting a message for
    // example)
    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // startup the event processing
        match args {
            ChannelInitializationParameter::OpenChannel(peer_id, channel_user_id, open_channel) => {
                debug!("Openning channel {:?}", &open_channel);

                let counterpart_pubkeys = (&open_channel).into();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    funding_type_script,
                    funding_amount,
                    to_self_delay,
                    first_per_commitment_point,
                    second_per_commitment_point,
                    ..
                } = &open_channel;

                if *chain_hash != [0u8; 32].into() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "Invalid chain hash {:?}",
                        chain_hash
                    ))));
                }

                if funding_type_script.is_some() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Funding type script is not none".to_string(),
                    )));
                }

                let seed = channel_user_id
                    .to_be_bytes()
                    .into_iter()
                    .chain(peer_id.as_bytes().iter().cloned())
                    .collect::<Vec<u8>>();

                let state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *to_self_delay,
                    counterpart_pubkeys,
                    first_per_commitment_point.clone(),
                    second_per_commitment_point.clone(),
                );

                let commitment_number = COUNTERPARTY_INITIAL_COMMITMENT_NUMBER;

                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: 0,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    to_self_delay: *to_self_delay,
                    funding_pubkey: state.signer.funding_key.pubkey(),
                    revocation_basepoint: state.signer.revocation_base_key.pubkey(),
                    payment_basepoint: state.signer.payment_key.pubkey(),
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    delayed_payment_basepoint: state.signer.delayed_payment_base_key.pubkey(),
                    tlc_basepoint: state.signer.tlc_base_key.pubkey(),
                    first_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number - 1),
                    next_local_nonce: state.signer.misig_nonce.public_nonce(),
                };

                let command = PCNMessageWithPeerId {
                    peer_id,
                    message: PCNMessage::AcceptChannel(accept_channel),
                };
                // TODO: maybe we should not use try_send here.
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(command),
                    ))
                    .expect("network actor alive");

                self.peer
                    .send_message(PeerActorMessage::ChannelCreated(*channel_id, myself))
                    .expect("peer actor alive");
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannelCommand(open_channel) => {
                info!("Trying to open a channel to {:?}", &open_channel.peer_id);

                let channel = open_channel.create_channel()?;

                let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
                let message = PCNMessage::OpenChannel(OpenChannel {
                    chain_hash: Hash256::default(),
                    channel_id: channel.id(),
                    funding_type_script: None,
                    funding_amount: channel.to_self_value,
                    funding_fee_rate: DEFAULT_FEE_RATE,
                    commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    to_self_delay: DEFAULT_TO_SELF_DELAY,
                    channel_flags: 0,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel.holder_channel_parameters.pubkeys.funding_pubkey,
                    revocation_basepoint: channel
                        .holder_channel_parameters
                        .pubkeys
                        .revocation_basepoint,
                    payment_basepoint: channel.holder_channel_parameters.pubkeys.payment_point,
                    delayed_payment_basepoint: channel
                        .holder_channel_parameters
                        .pubkeys
                        .delayed_payment_basepoint,
                    tlc_basepoint: channel.holder_channel_parameters.pubkeys.tlc_basepoint,
                    next_local_nonce: channel.signer.misig_nonce.public_nonce(),
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &open_channel.peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                            peer_id: open_channel.peer_id,
                            message,
                        }),
                    ))
                    .expect("network actor alive");
                self.peer
                    .send_message(PeerActorMessage::ChannelCreated(channel.id(), myself))
                    .expect("peer actor alive");
                Ok(channel)
            }
        }
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            PCNMessage::OpenChannel(_) => {
                panic!("OpenChannel message should be processed while prestarting")
            }
            PCNMessage::AcceptChannel(accept_channel) => {
                state.step(ChannelEvent::AcceptChannel(accept_channel))?
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelActorState {
    pub state: ChannelState,
    pub temp_id: Hash256,

    // Is this channel initially inbound?
    // An inbound channel is one where the counterparty is the funder of the channel.
    pub was_initially_inbound: bool,

    pub total_value: u64,
    pub to_self_value: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    pub holder_channel_parameters: ChannelParametersOneParty,

    // Commitment numbers that are used to derive keys.
    pub holder_commitment_number: u64,
    pub counterparty_commitment_number: u64,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).
    pub id: Option<Hash256>,
    pub counterparty_commitment_point: Option<Pubkey>,
    pub counterparty_prev_commitment_point: Option<Pubkey>,
    pub counterparty_channel_parameters: Option<ChannelParametersOneParty>,
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

impl ChannelActorState {
    pub fn id(&self) -> Hash256 {
        self.id.unwrap_or(self.temp_id)
    }
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
    InvalidChainHash(Hash256),
    #[error("Unsupported operation: {0}")]
    Unsupported(String),
    #[error("Invalid state")]
    InvalidState,
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Unimplemented operation: {0}")]
    Unimplemented(String),
    #[error("Failed to send command: {0}")]
    CommanderSendingError(#[from] TrySendError<NetworkActorCommand>),
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

fn new_channel_id_from_seed(seed: &[u8]) -> Hash256 {
    blake2b_256(seed).into()
}

fn derive_channel_id_from_revocation_keys(
    revocation_basepoint1: &Pubkey,
    revocation_basepoint2: &Pubkey,
) -> Hash256 {
    let holder_revocation = revocation_basepoint1.0.serialize();
    let counterparty_revocation = revocation_basepoint2.0.serialize();

    let preimage = if holder_revocation >= counterparty_revocation {
        counterparty_revocation
            .into_iter()
            .chain(holder_revocation.into_iter())
            .collect::<Vec<_>>()
    } else {
        holder_revocation
            .into_iter()
            .chain(counterparty_revocation.into_iter())
            .collect()
    };
    new_channel_id_from_seed(&preimage)
}

pub fn get_commitment_secret(commitment_seed: &[u8; 32], commitment_number: u64) -> [u8; 32] {
    // Note that here, we hold the same assumption to bolts for commitment number,
    // i.e. this number should be in the range [0, 2^48).
    let mut res: [u8; 32] = commitment_seed.clone();
    for i in 0..48 {
        let bitpos = 47 - i;
        if commitment_number & (1 << bitpos) == (1 << bitpos) {
            res[bitpos / 8] ^= 1 << (bitpos & 7);
            res = blake2b_256(&res);
        }
    }
    res
}

pub fn get_commitment_point(commitment_seed: &[u8; 32], commitment_number: u64) -> Pubkey {
    Privkey::from(&get_commitment_secret(commitment_seed, commitment_number)).pubkey()
}

impl ChannelActorState {
    pub fn new_inbound_channel<'a>(
        temp_channel_id: Hash256,
        seed: &[u8],
        counterparty_peer_id: PeerId,
        counterparty_value: u64,
        counterparty_delay: u64,
        counterparty_pubkeys: ChannelPublicKeys,
        counterparty_commitment_point: Pubkey,
        counterparty_prev_commitment_point: Pubkey,
    ) -> Self {
        let commitment_number = COUNTERPARTY_INITIAL_COMMITMENT_NUMBER;
        let signer = InMemorySigner::generate_from_seed(seed);
        let holder_pubkeys = signer.to_channel_public_keys(commitment_number);

        let channel_id = derive_channel_id_from_revocation_keys(
            &holder_pubkeys.revocation_basepoint,
            &counterparty_pubkeys.revocation_basepoint,
        );

        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
            was_initially_inbound: true,
            total_value: counterparty_value,
            temp_id: temp_channel_id,
            id: Some(channel_id),
            to_self_value: 0,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                selected_contest_delay: counterparty_delay as u64,
            },
            signer,
            counterparty_channel_parameters: Some(ChannelParametersOneParty {
                pubkeys: counterparty_pubkeys,
                selected_contest_delay: counterparty_delay,
            }),
            holder_commitment_number: commitment_number,
            counterparty_commitment_number: HOLDER_INITIAL_COMMITMENT_NUMBER,
            counterparty_commitment_point: Some(counterparty_commitment_point),
            counterparty_prev_commitment_point: Some(counterparty_prev_commitment_point),
        }
    }

    pub fn new_outbound_channel(
        seed: &[u8],
        value: u64,
        to_self_value: u64,
        to_self_delay: u64,
    ) -> Self {
        assert!(
            to_self_value <= value,
            "to_self_value must be less than or equal to value"
        );

        let new_channel_id = new_channel_id_from_seed(seed);
        let signer = InMemorySigner::generate_from_seed(seed);
        let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
        let holder_pubkeys = signer.to_channel_public_keys(commitment_number);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            funding_tx: None,
            was_initially_inbound: false,
            total_value: value,
            temp_id: new_channel_id.into(),
            id: None,
            to_self_value,
            signer,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                selected_contest_delay: to_self_delay,
            },
            counterparty_channel_parameters: None,
            holder_commitment_number: commitment_number,
            counterparty_commitment_number: COUNTERPARTY_INITIAL_COMMITMENT_NUMBER,
            counterparty_commitment_point: None,
            counterparty_prev_commitment_point: None,
        }
    }

    pub fn handle_accept_channel_message(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if accept_channel.funding_amount != self.total_value {
            return Err(ProcessingChannelError::InvalidParameter(
                "funding_amount mismatch".to_string(),
            ));
        }

        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState);
        }
        self.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
        let counterparty_pubkeys = (&accept_channel).into();
        self.counterparty_channel_parameters = Some(ChannelParametersOneParty {
            pubkeys: counterparty_pubkeys,
            selected_contest_delay: accept_channel.to_self_delay as u64,
        });

        debug!("OpenChannel: {:?}", &accept_channel);
        Ok(())
    }

    pub fn handle_commitment_signed_message(
        &mut self,
        commitment_signed: CommitmentSigned,
    ) -> ProcessingChannelResult {
        debug!("CommitmentSigned: {:?}", &commitment_signed);
        Ok(())
    }

    pub fn fill_in_channel_id(&mut self) {
        assert!(self.id.is_none(), "Channel id is already filled in");
        assert!(
            self.counterparty_channel_parameters.is_some(),
            "Counterparty pubkeys is required to derive actual channel id"
        );
        let counterparty_revocation = &self
            .counterparty_channel_parameters
            .as_ref()
            .unwrap()
            .pubkeys
            .revocation_basepoint;
        let holder_revocation = &self.holder_channel_parameters.pubkeys.revocation_basepoint;
        let channel_id =
            derive_channel_id_from_revocation_keys(&holder_revocation, &counterparty_revocation);

        self.id = Some(channel_id);
    }

    pub fn step(&mut self, event: ChannelEvent) -> ProcessingChannelResult {
        match event {
            ChannelEvent::AcceptChannel(accept_channel) => {
                self.handle_accept_channel_message(accept_channel)
            }
            ChannelEvent::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed_message(commitment_signed)
            }
            ChannelEvent::ChannelReady(channel_ready) => {
                match self.state {
                    ChannelState::FundingNegotiated => {
                        self.state = ChannelState::AwaitingChannelReady(
                            AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
                        );
                    }
                    ChannelState::AwaitingChannelReady(
                        AwaitingChannelReadyFlags::THEIR_CHANNEL_READY,
                    ) => {
                        self.state = ChannelState::ChannelReady(ChannelReadyFlags::empty());
                    }
                    _ => {
                        return Err(ProcessingChannelError::InvalidState);
                    }
                }
                debug!("ChannelReady: {:?}", &channel_ready);
                Ok(())
            }
        }
    }

    pub fn is_funded(&self) -> bool {
        match self.state {
            ChannelState::ChannelReady(_) => {
                assert!(self.funding_tx.is_some());
                assert!(self.id.is_some());
                assert!(self.counterparty_commitment_point.is_some());
                assert!(self.counterparty_prev_commitment_point.is_some());
                assert!(self.counterparty_channel_parameters.is_some());
                true
            }
            _ => false,
        }
    }

    pub fn must_get_funding_transaction(&self) -> &OutPoint {
        self.funding_tx.as_ref().expect("Channel is not funded")
    }

    pub fn build_commitment_tx(
        &self,
        commitment_number: u64,
        keys: &TxCreationKeys,
        local: bool,
    ) -> CommitmentTransaction {
        let directed_channel_parameters = self.must_get_directed_channel_parameters(local);
        // 1. Get transaction inputs from direct channel parameters.
        // 2. Assemble transaction outputs from current chanenl information.
        // 3. Sign the transaction with the private key.
        todo!("build_commitment_tx");
    }

    pub fn must_get_counterparty_channel_parameters(&self) -> &ChannelParametersOneParty {
        self.counterparty_channel_parameters
            .as_ref()
            .expect("Counterparty channel parameters")
    }

    pub fn must_get_fouded_channel_parameters(&self) -> FundedChannelParameters<'_> {
        FundedChannelParameters {
            holder: &self.holder_channel_parameters,
            counterparty: self.must_get_counterparty_channel_parameters(),
            funding_outpoint: self.must_get_funding_transaction(),
        }
    }

    pub fn must_get_directed_channel_parameters(
        &self,
        holder_is_broadcaster: bool,
    ) -> DirectedChannelTransactionParameters<'_> {
        DirectedChannelTransactionParameters {
            inner: self.must_get_fouded_channel_parameters(),
            holder_is_broadcaster,
        }
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
    ) -> Result<Transaction, ProcessingChannelError> {
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
    ) -> Result<TrustedCommitmentTransaction, ProcessingChannelError> {
        // This is the only field of the key cache that we trust
        let per_commitment_point = &self.keys.per_commitment_point;
        let keys = TxCreationKeys::from_channel_static_keys(
            per_commitment_point,
            broadcaster_keys,
            countersignatory_keys,
        );
        if keys != self.keys {
            return Err(ProcessingChannelError::InvalidParameter(
                "Key mismatch".to_string(),
            ));
        }
        let tx = self.internal_rebuild_transaction(
            &keys,
            channel_parameters,
            &broadcaster_keys.funding_pubkey,
            &countersignatory_keys.funding_pubkey,
        )?;
        if self.build_tx().as_bytes() != tx.as_bytes() {
            return Err(ProcessingChannelError::InvalidParameter(
                "Transaction mismatch".to_string(),
            ));
        }
        Ok(TrustedCommitmentTransaction { inner: self })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelParametersOneParty {
    pub pubkeys: ChannelPublicKeys,
    pub selected_contest_delay: u64,
}

pub struct FundedChannelParameters<'a> {
    pub holder: &'a ChannelParametersOneParty,
    pub counterparty: &'a ChannelParametersOneParty,
    pub funding_outpoint: &'a OutPoint,
}

/// Static channel fields used to build transactions given per-commitment fields, organized by
/// broadcaster/countersignatory.
///
/// This is derived from the holder/counterparty-organized ChannelTransactionParameters via the
/// as_holder_broadcastable and as_counterparty_broadcastable functions.
pub struct DirectedChannelTransactionParameters<'a> {
    /// The holder's channel static parameters
    inner: FundedChannelParameters<'a>,
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
    pub payment_hash: Hash256,
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

pub fn derive_private_key(secret: &Privkey, per_commitment_point: &Pubkey) -> Privkey {
    // TODO: Currently we only copy the input secret. We need to actually derive new private keys
    // from the per_commitment_point.
    secret.clone()
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
    pub tlc_base_key: Privkey,
    /// SecNonce used to generate valid signature in musig.
    pub misig_nonce: SecNonce,
    /// Seed to derive above keys (per commitment).
    pub commitment_seed: [u8; 32],
}

impl InMemorySigner {
    pub fn generate_from_seed(params: &[u8]) -> Self {
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
            let result = blake2b_hash_with_salt(seed, info);
            Privkey::from_slice(&result)
        };

        let funding_key = key_derive(&seed, b"funding key");
        let revocation_base_key = key_derive(funding_key.as_ref(), b"revocation base key");
        let payment_key = key_derive(revocation_base_key.as_ref(), b"payment key");
        let delayed_payment_base_key =
            key_derive(payment_key.as_ref(), b"delayed payment base key");
        let tlc_base_key = key_derive(delayed_payment_base_key.as_ref(), b"HTLC base key");
        let misig_nonce = key_derive(tlc_base_key.as_ref(), b"musig nocne");
        let misig_nonce = SecNonce::build(misig_nonce.as_ref()).build();

        Self {
            funding_key,
            revocation_base_key,
            payment_key,
            delayed_payment_base_key,
            tlc_base_key,
            misig_nonce,
            commitment_seed,
        }
    }

    fn to_channel_public_keys(&self, commitment_number: u64) -> ChannelPublicKeys {
        ChannelPublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            revocation_basepoint: self.derive_revocation_key(commitment_number).pubkey(),
            payment_point: self.derive_payment_key(commitment_number).pubkey(),
            delayed_payment_basepoint: self.derive_delayed_payment_key(commitment_number).pubkey(),
            tlc_basepoint: self.derive_tlc_key(commitment_number).pubkey(),
        }
    }

    pub fn get_commitment_point(&self, commitment_number: u64) -> Pubkey {
        get_commitment_point(&self.commitment_seed, commitment_number)
    }

    pub fn get_commitment_secret(&self, commitment_number: u64) -> [u8; 32] {
        get_commitment_secret(&self.commitment_seed, commitment_number)
    }

    pub fn derive_revocation_key(&self, commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(commitment_number);
        derive_private_key(&self.revocation_base_key, &per_commitment_point)
    }

    pub fn derive_payment_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.payment_key, &per_commitment_point)
    }

    pub fn derive_delayed_payment_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.delayed_payment_base_key, &per_commitment_point)
    }

    pub fn derive_tlc_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.tlc_base_key, &per_commitment_point)
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

#[cfg(test)]
mod tests {
    use crate::ckb::types::Privkey;

    use super::{derive_private_key, derive_public_key};

    #[test]
    fn test_derive_private_and_public_keys() {
        let privkey = Privkey::from(&[1; 32]);
        let per_commitment_point = Privkey::from(&[2; 32]).pubkey();
        let derived_privkey = derive_private_key(&privkey, &per_commitment_point);
        let derived_pubkey = derive_public_key(&privkey.pubkey(), &per_commitment_point);
        assert_eq!(derived_privkey.pubkey(), derived_pubkey);
    }
}
