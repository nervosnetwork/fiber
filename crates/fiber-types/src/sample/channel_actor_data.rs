/// StoreSample implementation for `ChannelActorData`.
use std::collections::{HashMap, VecDeque};

use crate::channel::{
    AddTlcCommand, ChannelActorData, ChannelBasePublicKeys, ChannelConstraints, ChannelState,
    ChannelTlcInfo, CommitmentNumbers, InMemorySigner, PendingTlcs, RemoveTlcFulfill,
    RemoveTlcReason, RetryableTlcOperation, RevokeAndAck, ShutdownInfo, TLCId, TlcInfo, TlcState,
    TlcStatus,
};
use crate::channel::{InboundTlcStatus, OutboundTlcStatus};
use crate::crate_time::SystemTime;
use crate::invoice::HashAlgorithm;
use crate::onion::PaymentOnionPacket;
use crate::protocol::SchnorrSignature;
use crate::protocol::{ChannelAnnouncement, ChannelUpdate};
use crate::schema::CHANNEL_ACTOR_STATE_PREFIX;
use crate::AppliedFlags;

use super::{
    deterministic_ecdsa_signature, deterministic_hash, deterministic_hash256,
    deterministic_privkey, deterministic_pubkey, deterministic_schnorr_signature,
    deterministic_xonly_pubkey, StoreSample,
};

impl StoreSample for ChannelActorData {
    const STORE_PREFIX: u8 = CHANNEL_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "ChannelActorData";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_minimal(seed), sample_full(seed)]
    }
}

/// Create a deterministic InMemorySigner from a seed and index.
fn deterministic_signer(seed: u64, index: u32) -> InMemorySigner {
    let funding_key = deterministic_privkey(seed, index);
    let tlc_base_key = deterministic_privkey(seed, index + 1);
    let musig2_base_nonce = deterministic_privkey(seed, index + 2);
    let commitment_seed = super::deterministic_hash(seed, index + 3);

    InMemorySigner {
        funding_key,
        tlc_base_key,
        musig2_base_nonce,
        commitment_seed,
    }
}

/// Create ChannelBasePublicKeys from signer.
fn channel_pubkeys_from_signer(signer: &InMemorySigner) -> ChannelBasePublicKeys {
    ChannelBasePublicKeys {
        funding_pubkey: signer.funding_key.pubkey(),
        tlc_base_key: signer.tlc_base_key.pubkey(),
    }
}

/// Create a deterministic PubNonce from a seed and index.
fn deterministic_pub_nonce(seed: u64, index: u32) -> musig2::PubNonce {
    use musig2::SecNonceBuilder;
    let hash = super::deterministic_hash(seed, index);
    let sec_nonce = SecNonceBuilder::new(hash).build();
    sec_nonce.public_nonce()
}

/// Sample 0: Minimal state — all Option fields are None, all collections empty, all defaults.
fn sample_minimal(seed: u64) -> ChannelActorData {
    let signer = deterministic_signer(seed, 0);
    let local_channel_public_keys = channel_pubkeys_from_signer(&signer);

    ChannelActorData {
        state: ChannelState::ChannelReady,
        public_channel_info: None,
        local_tlc_info: ChannelTlcInfo::default(),
        remote_tlc_info: None,
        local_pubkey: deterministic_pubkey(seed, 10),
        remote_pubkey: deterministic_pubkey(seed, 11),
        id: deterministic_hash256(seed, 0),
        funding_tx: None,
        funding_tx_confirmed_at: None,
        funding_udt_type_script: None,
        is_acceptor: false,
        is_one_way: false,
        to_local_amount: 0,
        to_remote_amount: 0,
        local_reserved_ckb_amount: 0,
        remote_reserved_ckb_amount: 0,
        commitment_fee_rate: 0,
        commitment_delay_epoch: 0,
        funding_fee_rate: 0,
        signer,
        local_channel_public_keys,
        commitment_numbers: CommitmentNumbers::default(),
        local_constraints: ChannelConstraints::default(),
        remote_constraints: ChannelConstraints::default(),
        tlc_state: TlcState {
            offered_tlcs: PendingTlcs {
                tlcs: vec![],
                next_tlc_id: 0,
            },
            received_tlcs: PendingTlcs {
                tlcs: vec![],
                next_tlc_id: 0,
            },
            waiting_ack: false,
        },
        retryable_tlc_operations: VecDeque::new(),
        waiting_forward_tlc_tasks: HashMap::new(),
        remote_shutdown_script: None,
        local_shutdown_script: ckb_types::packed::Script::default(),
        last_committed_remote_nonce: None,
        remote_revocation_nonce_for_verify: None,
        remote_revocation_nonce_for_send: None,
        remote_revocation_nonce_for_next: None,
        latest_commitment_transaction: None,
        remote_commitment_points: vec![],
        remote_channel_public_keys: None,
        local_shutdown_info: None,
        remote_shutdown_info: None,
        shutdown_transaction_hash: None,
        reestablishing: false,
        last_revoke_ack_msg: None,
        pending_replay_updates: vec![],
        last_was_revoke: false,
        created_at: SystemTime::UNIX_EPOCH,
    }
}

/// Sample 1: Full state — all Options are Some, all collections have entries.
fn sample_full(seed: u64) -> ChannelActorData {
    let signer = deterministic_signer(seed, 100);
    let local_channel_public_keys = channel_pubkeys_from_signer(&signer);
    let channel_id = deterministic_hash256(seed, 200);
    let outpoint = super::deterministic_outpoint(seed, 150);

    // --- PublicChannelInfo with ChannelAnnouncement and ChannelUpdate ---
    let channel_announcement = ChannelAnnouncement {
        node1_signature: Some(deterministic_ecdsa_signature(seed, 210)),
        node2_signature: Some(deterministic_ecdsa_signature(seed, 211)),
        ckb_signature: Some(SchnorrSignature(deterministic_schnorr_signature(seed, 212))),
        features: 0,
        chain_hash: deterministic_hash256(seed, 213),
        channel_outpoint: outpoint.clone(),
        node1_id: deterministic_pubkey(seed, 214),
        node2_id: deterministic_pubkey(seed, 215),
        ckb_key: deterministic_xonly_pubkey(seed, 216),
        capacity: 800_000_000_000,
        udt_type_script: Some(ckb_types::packed::Script::default()),
    };

    let channel_update = ChannelUpdate {
        signature: Some(deterministic_ecdsa_signature(seed, 220)),
        chain_hash: deterministic_hash256(seed, 213),
        channel_outpoint: outpoint,
        timestamp: 1_704_067_200,
        message_flags: crate::channel::ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
        channel_flags: crate::channel::ChannelUpdateChannelFlags::empty(),
        tlc_expiry_delta: 40,
        tlc_minimum_value: 1000,
        tlc_fee_proportional_millionths: 500,
    };

    let public_channel_info = crate::channel::PublicChannelInfo {
        local_channel_announcement_signature: Some((
            deterministic_ecdsa_signature(seed, 200),
            musig2::PartialSignature::from_slice(&[2u8; 32]).unwrap(),
        )),
        remote_channel_announcement_signature: Some((
            deterministic_ecdsa_signature(seed, 201),
            musig2::PartialSignature::from_slice(&[2u8; 32]).unwrap(),
        )),
        remote_channel_announcement_nonce: Some(deterministic_pub_nonce(seed, 202)),
        channel_announcement: Some(channel_announcement),
        channel_update: Some(channel_update),
    };

    // --- TLCs with all fields populated ---
    let offered_tlc = TlcInfo {
        status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        tlc_id: TLCId::Offered(0),
        amount: 50_000_000,
        payment_hash: deterministic_hash256(seed, 300),
        total_amount: Some(100_000_000),
        payment_secret: Some(deterministic_hash256(seed, 301)),
        attempt_id: Some(1),
        expiry: 1_704_070_800,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: Some(PaymentOnionPacket::new(
            deterministic_hash(seed, 302).to_vec(),
        )),
        shared_secret: deterministic_hash(seed, 303),
        is_trampoline_hop: true,
        created_at: CommitmentNumbers::default(),
        removed_reason: Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: deterministic_hash256(seed, 304),
        })),
        forwarding_tlc: Some((deterministic_hash256(seed, 305), 42)),
        removed_confirmed_at: Some(5),
        applied_flags: AppliedFlags::ADD | AppliedFlags::REMOVE,
    };

    let received_tlc = TlcInfo {
        status: TlcStatus::Inbound(InboundTlcStatus::Committed),
        tlc_id: TLCId::Received(0),
        amount: 30_000_000,
        payment_hash: deterministic_hash256(seed, 310),
        total_amount: Some(60_000_000),
        payment_secret: Some(deterministic_hash256(seed, 311)),
        attempt_id: Some(2),
        expiry: 1_704_074_400,
        hash_algorithm: HashAlgorithm::Sha256,
        onion_packet: Some(PaymentOnionPacket::new(
            deterministic_hash(seed, 312).to_vec(),
        )),
        shared_secret: deterministic_hash(seed, 313),
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::default(),
        removed_reason: Some(RemoveTlcReason::RemoveTlcFail(crate::onion::TlcErrPacket {
            onion_packet: deterministic_hash(seed, 314).to_vec(),
        })),
        forwarding_tlc: Some((deterministic_hash256(seed, 315), 7)),
        removed_confirmed_at: Some(10),
        applied_flags: AppliedFlags::ADD,
    };

    // --- RetryableTlcOperations: non-empty ---
    let retryable_ops = VecDeque::from(vec![
        RetryableTlcOperation::RemoveTlc(
            TLCId::Received(0),
            RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                payment_preimage: deterministic_hash256(seed, 350),
            }),
        ),
        RetryableTlcOperation::AddTlc(AddTlcCommand {
            amount: 10_000_000,
            payment_hash: deterministic_hash256(seed, 351),
            attempt_id: Some(3),
            expiry: 1_704_078_000,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: Some(PaymentOnionPacket::new(
                deterministic_hash(seed, 352).to_vec(),
            )),
            shared_secret: deterministic_hash(seed, 353),
            is_trampoline_hop: false,
            previous_tlc: Some(crate::channel::PrevTlcInfo::new_with_shared_secret(
                deterministic_hash256(seed, 354),
                5,
                1000,
                deterministic_hash(seed, 355),
            )),
        }),
    ]);

    // --- waiting_forward_tlc_tasks: non-empty ---
    let mut waiting_forward = HashMap::new();
    waiting_forward.insert(TLCId::Received(0), deterministic_hash(seed, 360));

    // --- RevokeAndAck: all fields populated ---
    let revoke_and_ack = RevokeAndAck {
        channel_id,
        revocation_partial_signature: musig2::PartialSignature::from_slice(&[2u8; 32]).unwrap(),
        next_per_commitment_point: deterministic_pubkey(seed, 810),
        next_revocation_nonce: deterministic_pub_nonce(seed, 811),
    };

    // --- ShutdownInfo for both local and remote ---
    let shutdown_info = ShutdownInfo {
        close_script: ckb_types::packed::Script::default(),
        fee_rate: 2000,
        signature: Some(musig2::PartialSignature::from_slice(&[2u8; 32]).unwrap()),
    };

    ChannelActorData {
        state: ChannelState::ChannelReady,
        public_channel_info: Some(public_channel_info),
        local_tlc_info: ChannelTlcInfo {
            enabled: true,
            timestamp: 1_704_067_200_000,
            tlc_fee_proportional_millionths: 500,
            tlc_expiry_delta: 40,
            tlc_minimum_value: 1000,
        },
        remote_tlc_info: Some(ChannelTlcInfo {
            enabled: true,
            timestamp: 1_704_067_200_000,
            tlc_fee_proportional_millionths: 600,
            tlc_expiry_delta: 40,
            tlc_minimum_value: 2000,
        }),
        local_pubkey: deterministic_pubkey(seed, 110),
        remote_pubkey: deterministic_pubkey(seed, 111),
        id: channel_id,
        funding_tx: Some(ckb_types::packed::Transaction::default()),
        funding_tx_confirmed_at: Some((
            ckb_types::H256(super::deterministic_hash(seed, 201)),
            100,
            1_704_067_200,
        )),
        funding_udt_type_script: Some(ckb_types::packed::Script::default()),
        is_acceptor: true,
        is_one_way: true,
        to_local_amount: 500_000_000_000,
        to_remote_amount: 300_000_000_000,
        local_reserved_ckb_amount: 6_200_000_000,
        remote_reserved_ckb_amount: 6_200_000_000,
        commitment_fee_rate: 1000,
        commitment_delay_epoch: 40,
        funding_fee_rate: 1000,
        signer,
        local_channel_public_keys,
        commitment_numbers: CommitmentNumbers::default(),
        local_constraints: ChannelConstraints::default(),
        remote_constraints: ChannelConstraints::default(),
        tlc_state: TlcState {
            offered_tlcs: PendingTlcs {
                tlcs: vec![offered_tlc],
                next_tlc_id: 1,
            },
            received_tlcs: PendingTlcs {
                tlcs: vec![received_tlc],
                next_tlc_id: 1,
            },
            waiting_ack: true,
        },
        retryable_tlc_operations: retryable_ops,
        waiting_forward_tlc_tasks: waiting_forward,
        remote_shutdown_script: Some(ckb_types::packed::Script::default()),
        local_shutdown_script: ckb_types::packed::Script::default(),
        last_committed_remote_nonce: Some(deterministic_pub_nonce(seed, 400)),
        remote_revocation_nonce_for_verify: Some(deterministic_pub_nonce(seed, 401)),
        remote_revocation_nonce_for_send: Some(deterministic_pub_nonce(seed, 402)),
        remote_revocation_nonce_for_next: Some(deterministic_pub_nonce(seed, 403)),
        latest_commitment_transaction: Some(ckb_types::packed::Transaction::default()),
        remote_commitment_points: vec![
            (0, deterministic_pubkey(seed, 500)),
            (1, deterministic_pubkey(seed, 501)),
        ],
        remote_channel_public_keys: Some(ChannelBasePublicKeys {
            funding_pubkey: deterministic_pubkey(seed, 600),
            tlc_base_key: deterministic_pubkey(seed, 601),
        }),
        local_shutdown_info: Some(shutdown_info.clone()),
        remote_shutdown_info: Some(shutdown_info),
        shutdown_transaction_hash: Some(ckb_types::H256(super::deterministic_hash(seed, 950))),
        reestablishing: true,
        last_revoke_ack_msg: Some(revoke_and_ack),
        pending_replay_updates: vec![],
        last_was_revoke: true,
        created_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_704_067_200_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_actor_data_samples_roundtrip() {
        ChannelActorData::verify_samples_roundtrip(42);
    }
}
