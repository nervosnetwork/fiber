use crate::fiber::channel::TlcInfo;
use crate::fiber::channel::{
    CommitmentNumbers, InboundTlcStatus, OutboundTlcStatus, TLCId, TlcState, TlcStatus,
};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::RemoveTlcFulfill;
use crate::fiber::types::{Hash256, NO_SHARED_SECRET};
use crate::fiber::types::{PaymentOnionPacket, RemoveTlcReason};
use crate::gen_rand_sha256_hash;
use crate::now_timestamp_as_millis_u64;
use ckb_hash::new_blake2b;
use ckb_types::packed::Byte32;
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use std::collections::HashMap;

fn sign_tlcs<'a>(tlcs: impl Iterator<Item = &'a TlcInfo>) -> Hash256 {
    // serialize active_tls to ge a hash
    let mut keyparts = tlcs
        .map(|tlc| (tlc.amount, tlc.payment_hash))
        .collect::<Vec<_>>();

    keyparts.sort_by(|a, b| {
        let a: Byte32 = a.1.into();
        let b: Byte32 = b.1.into();
        a.cmp(&b)
    });

    eprintln!("keyparts: {:?}", keyparts);
    let serialized = serde_json::to_string(&keyparts).expect("Failed to serialize tls");

    // Hash the serialized data using SHA-256
    let mut hasher = new_blake2b();
    hasher.update(serialized.to_string().as_bytes());
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);

    result.into()
}

pub struct TlcActorState {
    pub tlc_state: TlcState,
    pub peer_id: String,
}

impl TlcActorState {
    pub fn get_peer(&self) -> String {
        if self.peer_id == "peer_a" {
            "peer_b".to_string()
        } else {
            "peer_a".to_string()
        }
    }
}

pub struct NetworkActorState {
    network: ActorRef<NetworkActorMessage>,
    pub peers: HashMap<String, ActorRef<TlcActorMessage>>,
}

impl NetworkActorState {
    pub async fn add_peer(&mut self, peer_id: String) {
        let network = self.network.clone();
        let actor = Actor::spawn_linked(
            Some(peer_id.clone()),
            TlcActor::new(network.clone()),
            peer_id.clone(),
            network.clone().get_cell(),
        )
        .await
        .expect("Failed to start tlc actor")
        .0;
        self.peers.insert(peer_id.clone(), actor);
        eprintln!("add_peer: {:?} added successfully ...", peer_id);
    }
}

pub struct TlcActor {
    network: ActorRef<NetworkActorMessage>,
}

impl TlcActor {
    pub fn new(network: ActorRef<NetworkActorMessage>) -> Self {
        Self { network }
    }
}

#[derive(Debug, Clone)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    pub onion_packet: Option<PaymentOnionPacket>,
    pub shared_secret: [u8; 32],
    #[allow(dead_code)]
    pub previous_tlc: Option<(Hash256, u64)>,
}

pub struct NetworkActor {}

#[derive(Debug)]
pub enum TlcActorMessage {
    Debug,
    CommandAddTlc(AddTlcCommand),
    CommandRemoveTlc(u64),
    PeerAddTlc(TlcInfo),
    PeerRemoveTlc(u64),
    PeerCommitmentSigned(Hash256),
    PeerRevokeAndAck(Hash256),
    //PeerRemoveTlc,
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    RegisterPeer(String),
    AddTlc(String, AddTlcCommand),
    RemoveTlc(String, u64),
    PeerMsg(String, TlcActorMessage),
}

#[rasync_trait]
impl Actor for NetworkActor {
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = ();

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            NetworkActorMessage::RegisterPeer(peer_id) => {
                state.add_peer(peer_id).await;
            }
            NetworkActorMessage::AddTlc(peer_id, add_tlc) => {
                eprintln!("NetworkActorMessage::AddTlc");
                if let Some(actor) = state.peers.get(&peer_id) {
                    actor
                        .send_message(TlcActorMessage::CommandAddTlc(add_tlc))
                        .expect("send ok");
                }
            }
            NetworkActorMessage::RemoveTlc(peer_id, tlc_id) => {
                if let Some(actor) = state.peers.get(&peer_id) {
                    actor
                        .send_message(TlcActorMessage::CommandRemoveTlc(tlc_id))
                        .expect("send ok");
                }
            }
            NetworkActorMessage::PeerMsg(peer_id, peer_msg) => {
                if let Some(actor) = state.peers.get(&peer_id) {
                    eprintln!("NetworkActorMessage::PeerMsg: {:?}", peer_msg);
                    actor.send_message(peer_msg).expect("send ok");
                }
            }
        }
        Ok(())
    }

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        eprintln!("NetworkActor pre_start");
        Ok(NetworkActorState {
            peers: Default::default(),
            network: myself.clone(),
        })
    }
}

#[rasync_trait]
impl Actor for TlcActor {
    type Msg = TlcActorMessage;
    type State = TlcActorState;
    type Arguments = String;

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            TlcActorMessage::Debug => {
                eprintln!("Peer {} Debug", state.peer_id);
                for tlc in state.tlc_state.offered_tlcs.tlcs.iter() {
                    eprintln!("offered_tlc: {:?}", tlc.log());
                }
                for tlc in state.tlc_state.received_tlcs.tlcs.iter() {
                    eprintln!("received_tlc: {:?}", tlc.log());
                }
            }
            TlcActorMessage::CommandAddTlc(command) => {
                eprintln!(
                    "Peer {} TlcActorMessage::Command_AddTlc: {:?}",
                    state.peer_id, command
                );
                let next_offer_id = state.tlc_state.get_next_offering();
                let add_tlc = TlcInfo {
                    channel_id: gen_rand_sha256_hash(),
                    tlc_id: TLCId::Offered(next_offer_id),
                    amount: command.amount,
                    payment_hash: command.payment_hash,
                    expiry: command.expiry,
                    hash_algorithm: command.hash_algorithm,
                    created_at: CommitmentNumbers::default(),
                    removed_reason: None,
                    onion_packet: command.onion_packet,
                    shared_secret: command.shared_secret,
                    previous_tlc: None,
                    status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
                };
                state.tlc_state.add_offered_tlc(add_tlc.clone());
                state.tlc_state.increment_offering();
                let peer = state.get_peer();
                self.network
                    .send_message(NetworkActorMessage::PeerMsg(
                        peer.clone(),
                        TlcActorMessage::PeerAddTlc(add_tlc),
                    ))
                    .expect("send ok");

                // send commitment signed
                let tlcs = state.tlc_state.commitment_signed_tlcs(false);
                let hash = sign_tlcs(tlcs);
                eprintln!("got hash: {:?}", hash);
                self.network
                    .send_message(NetworkActorMessage::PeerMsg(
                        peer,
                        TlcActorMessage::PeerCommitmentSigned(hash),
                    ))
                    .expect("send ok");
            }
            TlcActorMessage::CommandRemoveTlc(tlc_id) => {
                eprintln!("Peer {} process remove tlc ....", state.peer_id);
                state.tlc_state.set_received_tlc_removed(
                    tlc_id,
                    RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                        payment_preimage: Default::default(),
                    }),
                );
                let peer = state.get_peer();
                self.network
                    .send_message(NetworkActorMessage::PeerMsg(
                        peer.clone(),
                        TlcActorMessage::PeerRemoveTlc(tlc_id),
                    ))
                    .expect("send ok");

                // send commitment signed
                let tlcs = state.tlc_state.commitment_signed_tlcs(false);
                let hash = sign_tlcs(tlcs);
                eprintln!("got hash: {:?}", hash);
                self.network
                    .send_message(NetworkActorMessage::PeerMsg(
                        peer,
                        TlcActorMessage::PeerCommitmentSigned(hash),
                    ))
                    .expect("send ok");
            }
            TlcActorMessage::PeerAddTlc(add_tlc) => {
                eprintln!(
                    "Peer {} process peer add_tlc .... with tlc_id: {:?}",
                    state.peer_id, add_tlc.tlc_id
                );
                let mut tlc = add_tlc.clone();
                tlc.flip_mut();
                tlc.status = TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced);
                state.tlc_state.add_received_tlc(tlc);
                eprintln!("add peer tlc successfully: {:?}", add_tlc);
            }
            TlcActorMessage::PeerRemoveTlc(tlc_id) => {
                eprintln!(
                    "Peer {} process peer remove tlc .... with tlc_id: {}",
                    state.peer_id, tlc_id
                );
                state.tlc_state.set_offered_tlc_removed(
                    tlc_id,
                    RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                        payment_preimage: Default::default(),
                    }),
                );
            }
            TlcActorMessage::PeerCommitmentSigned(peer_hash) => {
                eprintln!(
                    "\nPeer {} processed peer commitment_signed ....",
                    state.peer_id
                );
                let tlcs = state.tlc_state.commitment_signed_tlcs(true);
                let hash = sign_tlcs(tlcs);
                assert_eq!(hash, peer_hash);

                let peer = state.get_peer();

                state.tlc_state.update_for_commitment_signed();

                eprintln!("sending peer revoke and ack ....");
                let tlcs = state.tlc_state.commitment_signed_tlcs(false);
                let hash = sign_tlcs(tlcs);
                self.network
                    .send_message(NetworkActorMessage::PeerMsg(
                        peer.clone(),
                        TlcActorMessage::PeerRevokeAndAck(hash),
                    ))
                    .expect("send ok");

                // send commitment signed from our side if necessary
                if state.tlc_state.need_another_commitment_signed() {
                    eprintln!("sending another commitment signed ....");
                    let tlcs = state.tlc_state.commitment_signed_tlcs(false);
                    let hash = sign_tlcs(tlcs);
                    self.network
                        .send_message(NetworkActorMessage::PeerMsg(
                            peer,
                            TlcActorMessage::PeerCommitmentSigned(hash),
                        ))
                        .expect("send ok");
                }
            }
            TlcActorMessage::PeerRevokeAndAck(peer_hash) => {
                eprintln!("Peer {} processed peer revoke and ack ....", state.peer_id);
                let tlcs = state.tlc_state.commitment_signed_tlcs(true);
                let hash = sign_tlcs(tlcs);
                assert_eq!(hash, peer_hash);

                state.tlc_state.update_for_revoke_and_ack_peer_message();
            }
        }
        Ok(())
    }

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        eprintln!("TlcActor pre_start");
        match args {
            peer_id => {
                eprintln!("peer_id: {:?}", peer_id);
                Ok(TlcActorState {
                    tlc_state: Default::default(),
                    peer_id,
                })
            }
        }
    }
}

#[tokio::test]
async fn test_tlc_actor() {
    let (network_actor, _handle) = Actor::spawn(None, NetworkActor {}, ())
        .await
        .expect("Failed to start tlc actor");
    network_actor
        .send_message(NetworkActorMessage::RegisterPeer("peer_a".to_string()))
        .unwrap();
    network_actor
        .send_message(NetworkActorMessage::RegisterPeer("peer_b".to_string()))
        .unwrap();

    network_actor
        .send_message(NetworkActorMessage::AddTlc(
            "peer_a".to_string(),
            AddTlcCommand {
                amount: 10000,
                payment_hash: gen_rand_sha256_hash(),
                expiry: now_timestamp_as_millis_u64() + 1000,
                hash_algorithm: HashAlgorithm::Sha256,
                onion_packet: None,
                shared_secret: NO_SHARED_SECRET.clone(),
                previous_tlc: None,
            },
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    network_actor
        .send_message(NetworkActorMessage::AddTlc(
            "peer_a".to_string(),
            AddTlcCommand {
                amount: 20000,
                payment_hash: gen_rand_sha256_hash(),
                expiry: now_timestamp_as_millis_u64() + 1000,
                hash_algorithm: HashAlgorithm::Sha256,
                onion_packet: None,
                shared_secret: NO_SHARED_SECRET.clone(),
                previous_tlc: None,
            },
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    network_actor
        .send_message(NetworkActorMessage::AddTlc(
            "peer_b".to_string(),
            AddTlcCommand {
                amount: 30000,
                payment_hash: gen_rand_sha256_hash(),
                expiry: now_timestamp_as_millis_u64() + 1000,
                hash_algorithm: HashAlgorithm::Sha256,
                onion_packet: None,
                shared_secret: NO_SHARED_SECRET.clone(),
                previous_tlc: None,
            },
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    network_actor
        .send_message(NetworkActorMessage::AddTlc(
            "peer_b".to_string(),
            AddTlcCommand {
                amount: 50000,
                payment_hash: gen_rand_sha256_hash(),
                expiry: now_timestamp_as_millis_u64() + 1000,
                hash_algorithm: HashAlgorithm::Sha256,
                onion_packet: None,
                shared_secret: NO_SHARED_SECRET.clone(),
                previous_tlc: None,
            },
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    // remove tlc from peer_b
    network_actor
        .send_message(NetworkActorMessage::RemoveTlc("peer_b".to_string(), 0))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    network_actor
        .send_message(NetworkActorMessage::PeerMsg(
            "peer_a".to_string(),
            TlcActorMessage::Debug,
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    network_actor
        .send_message(NetworkActorMessage::PeerMsg(
            "peer_b".to_string(),
            TlcActorMessage::Debug,
        ))
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
}

#[test]
fn test_tlc_state_v2() {
    let mut tlc_state = TlcState::default();
    let mut add_tlc1 = TlcInfo {
        amount: 10000,
        status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        channel_id: gen_rand_sha256_hash(),
        payment_hash: gen_rand_sha256_hash(),
        expiry: now_timestamp_as_millis_u64() + 1000,
        hash_algorithm: HashAlgorithm::Sha256,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET.clone(),
        tlc_id: TLCId::Offered(0),
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        previous_tlc: None,
    };
    let mut add_tlc2 = TlcInfo {
        amount: 20000,
        status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        channel_id: gen_rand_sha256_hash(),
        payment_hash: gen_rand_sha256_hash(),
        expiry: now_timestamp_as_millis_u64() + 2000,
        hash_algorithm: HashAlgorithm::Sha256,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET.clone(),
        tlc_id: TLCId::Offered(1),
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        previous_tlc: None,
    };
    tlc_state.add_offered_tlc(add_tlc1.clone());
    tlc_state.add_offered_tlc(add_tlc2.clone());

    let mut tlc_state_2 = TlcState::default();
    add_tlc1.flip_mut();
    add_tlc2.flip_mut();
    add_tlc1.status = TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced);
    add_tlc2.status = TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced);
    tlc_state_2.add_received_tlc(add_tlc1);
    tlc_state_2.add_received_tlc(add_tlc2);

    let hash1 = sign_tlcs(tlc_state.commitment_signed_tlcs(true));
    eprintln!("hash1: {:?}", hash1);

    let hash2 = sign_tlcs(tlc_state_2.commitment_signed_tlcs(false));
    eprintln!("hash2: {:?}", hash2);
    assert_eq!(hash1, hash2);
}
