/// StoreSample implementation for `ChannelActorState`.
///
/// Provides deterministic sample instances for migration testing.
use std::collections::{HashMap, VecDeque};

use crate::time::{Duration, SystemTime, UNIX_EPOCH};

use ckb_types::packed::{Script, Transaction};
use musig2::secp::MaybeScalar;
use musig2::SecNonceBuilder;

use crate::fiber::channel::InMemorySignerExt;
use crate::fiber::channel::{
    AddTlcCommand, AppliedFlags, ChannelActorData, ChannelActorState, ChannelBasePublicKeys,
    ChannelConstraints, ChannelState, ChannelTlcInfo, CommitmentNumbers, InMemorySigner,
    PendingTlcs, PrevTlcInfo, PublicChannelInfo, RetryableTlcOperation, ShutdownInfo, TLCId,
    TlcInfo, TlcState, TlcStatus,
};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::{
    ChannelAnnouncement, ChannelUpdate, ChannelUpdateChannelFlags, ChannelUpdateMessageFlags,
    PaymentOnionPacket, RemoveTlcFulfill, RemoveTlcReason, RevokeAndAck, SchnorrSignature,
    TlcErrPacket,
};
use crate::store::schema::CHANNEL_ACTOR_STATE_PREFIX;

use super::{
    deterministic_ecdsa_signature, deterministic_hash, deterministic_hash256,
    deterministic_outpoint, deterministic_pubkey, deterministic_schnorr_signature,
    deterministic_xonly_pubkey, StoreSample,
};

/// Create a deterministic SystemTime anchored to a fixed epoch.
fn deterministic_time(offset_secs: u64) -> SystemTime {
    // Use a fixed reference point: 2024-01-01T00:00:00Z (Unix timestamp 1704067200)
    UNIX_EPOCH + Duration::from_secs(1_704_067_200 + offset_secs)
}

/// Create a deterministic PubNonce from a seed and index.
fn deterministic_pub_nonce(seed: u64, index: u32) -> musig2::PubNonce {
    let hash = deterministic_hash(seed, index);
    let sec_nonce = SecNonceBuilder::new(hash).build();
    sec_nonce.public_nonce()
}

/// Create a deterministic InMemorySigner from a seed and index.
fn deterministic_signer(seed: u64, index: u32) -> InMemorySigner {
    let params = deterministic_hash(seed, index);
    InMemorySigner::generate_from_seed(&params)
}

/// Create ChannelBasePublicKeys from an InMemorySigner's public keys.
fn channel_pubkeys_from_signer(signer: &InMemorySigner) -> ChannelBasePublicKeys {
    ChannelBasePublicKeys {
        funding_pubkey: signer.funding_key.pubkey(),
        tlc_base_key: signer.tlc_base_key.pubkey(),
    }
}

/// Create a deterministic ChannelBasePublicKeys from a seed and index.
fn deterministic_channel_pubkeys(seed: u64, base_index: u32) -> ChannelBasePublicKeys {
    ChannelBasePublicKeys {
        funding_pubkey: deterministic_pubkey(seed, base_index),
        tlc_base_key: deterministic_pubkey(seed, base_index + 1),
    }
}

impl StoreSample for ChannelActorState {
    const STORE_PREFIX: u8 = CHANNEL_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "ChannelActorState";

    fn samples(seed: u64) -> Vec<Self> {
        vec![Self::sample_minimal(seed), Self::sample_full(seed)]
    }
}

impl ChannelActorState {
    /// Sample 0: Minimal state — all Option fields are None, all collections empty, all defaults.
    fn sample_minimal(seed: u64) -> Self {
        let signer = deterministic_signer(seed, 0);
        let local_channel_public_keys = channel_pubkeys_from_signer(&signer);

        ChannelActorState {
            core: ChannelActorData {
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
                tlc_state: Default::default(),
                retryable_tlc_operations: VecDeque::new(),
                waiting_forward_tlc_tasks: HashMap::new(),
                remote_shutdown_script: None,
                local_shutdown_script: Script::default(),
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
                created_at: deterministic_time(0),
            },
            // #[serde(skip)] fields — not serialized, use defaults
            waiting_peer_response: None,
            network: None,
            scheduled_channel_update_handle: None,
            pending_notify_settle_tlcs: vec![],
            ephemeral_config: Default::default(),
            private_key: None,
        }
    }

    /// Sample 1: Fully populated state — every Option field is Some, every nested
    /// Option is Some, every collection is non-empty. This is the most comprehensive
    /// test for bincode serialization compatibility across versions.
    fn sample_full(seed: u64) -> Self {
        let signer = deterministic_signer(seed, 100);
        let local_channel_public_keys = channel_pubkeys_from_signer(&signer);
        let channel_id = deterministic_hash256(seed, 100);
        let outpoint = deterministic_outpoint(seed, 150);

        // --- PublicChannelInfo: all fields Some ---
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
            udt_type_script: Some(Script::default()),
        };

        let channel_update = ChannelUpdate {
            signature: Some(deterministic_ecdsa_signature(seed, 220)),
            chain_hash: deterministic_hash256(seed, 213),
            channel_outpoint: outpoint,
            timestamp: 1_704_067_200,
            message_flags: ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
            channel_flags: ChannelUpdateChannelFlags::empty(),
            tlc_expiry_delta: 40,
            tlc_minimum_value: 1000,
            tlc_fee_proportional_millionths: 500,
        };

        let public_channel_info = PublicChannelInfo {
            local_channel_announcement_signature: Some((
                deterministic_ecdsa_signature(seed, 200),
                MaybeScalar::two(),
            )),
            remote_channel_announcement_signature: Some((
                deterministic_ecdsa_signature(seed, 201),
                MaybeScalar::two(),
            )),
            remote_channel_announcement_nonce: Some(deterministic_pub_nonce(seed, 202)),
            channel_announcement: Some(channel_announcement),
            channel_update: Some(channel_update),
        };

        // --- TlcState: with offered and received TLCs, all nested Options populated ---
        let offered_tlc = TlcInfo {
            status: TlcStatus::Outbound(crate::fiber::channel::OutboundTlcStatus::LocalAnnounced),
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
            status: TlcStatus::Inbound(crate::fiber::channel::InboundTlcStatus::Committed),
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
            removed_reason: Some(RemoveTlcReason::RemoveTlcFail(TlcErrPacket {
                onion_packet: deterministic_hash(seed, 314).to_vec(),
            })),
            forwarding_tlc: Some((deterministic_hash256(seed, 315), 7)),
            removed_confirmed_at: Some(10),
            applied_flags: AppliedFlags::ADD,
        };

        let tlc_state = TlcState {
            offered_tlcs: PendingTlcs {
                tlcs: vec![offered_tlc],
                next_tlc_id: 1,
            },
            received_tlcs: PendingTlcs {
                tlcs: vec![received_tlc],
                next_tlc_id: 1,
            },
            waiting_ack: true,
        };

        // --- RetryableTlcOperations: non-empty with both variants ---
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
                previous_tlc: Some(PrevTlcInfo::new_with_shared_secret(
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
            revocation_partial_signature: MaybeScalar::two(),
            next_per_commitment_point: deterministic_pubkey(seed, 810),
            next_revocation_nonce: deterministic_pub_nonce(seed, 811),
        };

        ChannelActorState {
            core: ChannelActorData {
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
                funding_tx: Some(Transaction::default()),
                funding_tx_confirmed_at: Some((
                    ckb_types::H256(deterministic_hash(seed, 151)),
                    100,
                    1_704_067_200,
                )),
                funding_udt_type_script: Some(Script::default()),
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
                local_constraints: ChannelConstraints::new(100_000_000_000, 30),
                remote_constraints: ChannelConstraints::new(100_000_000_000, 30),
                tlc_state,
                retryable_tlc_operations: retryable_ops,
                waiting_forward_tlc_tasks: waiting_forward,
                remote_shutdown_script: Some(Script::default()),
                local_shutdown_script: Script::default(),
                last_committed_remote_nonce: Some(deterministic_pub_nonce(seed, 400)),
                remote_revocation_nonce_for_verify: Some(deterministic_pub_nonce(seed, 401)),
                remote_revocation_nonce_for_send: Some(deterministic_pub_nonce(seed, 402)),
                remote_revocation_nonce_for_next: Some(deterministic_pub_nonce(seed, 403)),
                latest_commitment_transaction: Some(Transaction::default()),
                remote_commitment_points: vec![
                    (0, deterministic_pubkey(seed, 500)),
                    (1, deterministic_pubkey(seed, 501)),
                ],
                remote_channel_public_keys: Some(deterministic_channel_pubkeys(seed, 600)),
                local_shutdown_info: Some(ShutdownInfo {
                    close_script: Script::default(),
                    fee_rate: 2000,
                    signature: Some(MaybeScalar::two()),
                }),
                remote_shutdown_info: Some(ShutdownInfo {
                    close_script: Script::default(),
                    fee_rate: 2000,
                    signature: Some(MaybeScalar::two()),
                }),
                shutdown_transaction_hash: Some(ckb_types::H256(deterministic_hash(seed, 950))),
                reestablishing: true,
                last_revoke_ack_msg: Some(revoke_and_ack),
                created_at: deterministic_time(3600),
            },
            // #[serde(skip)] fields — not serialized, use defaults
            waiting_peer_response: None,
            network: None,
            scheduled_channel_update_handle: None,
            pending_notify_settle_tlcs: vec![],
            ephemeral_config: Default::default(),
            private_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_actor_state_samples_roundtrip() {
        ChannelActorState::verify_samples_roundtrip(42);
    }

    #[test]
    fn test_channel_actor_state_samples_deterministic() {
        let bytes_a = ChannelActorState::sample_bytes(42);
        let bytes_b = ChannelActorState::sample_bytes(42);
        assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
    }

    #[test]
    fn test_channel_actor_state_different_seeds() {
        let bytes_42 = ChannelActorState::sample_bytes(42);
        let bytes_99 = ChannelActorState::sample_bytes(99);
        assert_ne!(
            bytes_42, bytes_99,
            "Different seeds should produce different bytes"
        );
    }

    #[test]
    fn test_channel_actor_state_sample_count() {
        let samples = ChannelActorState::samples(42);
        assert_eq!(samples.len(), 2, "Should produce 2 sample variants");
    }

    #[test]
    fn test_channel_actor_state_full_no_none() {
        let full = ChannelActorState::sample_full(42);

        // --- Top-level Option fields ---
        assert!(full.public_channel_info.is_some());
        assert!(full.remote_tlc_info.is_some());
        assert!(full.funding_tx.is_some());
        assert!(full.funding_tx_confirmed_at.is_some());
        assert!(full.funding_udt_type_script.is_some());
        assert!(full.remote_shutdown_script.is_some());
        assert!(full.last_committed_remote_nonce.is_some());
        assert!(full.remote_revocation_nonce_for_verify.is_some());
        assert!(full.remote_revocation_nonce_for_send.is_some());
        assert!(full.remote_revocation_nonce_for_next.is_some());
        assert!(full.latest_commitment_transaction.is_some());
        assert!(full.remote_channel_public_keys.is_some());
        assert!(full.local_shutdown_info.is_some());
        assert!(full.remote_shutdown_info.is_some());
        assert!(full.shutdown_transaction_hash.is_some());
        assert!(full.last_revoke_ack_msg.is_some());

        // --- PublicChannelInfo nested Options ---
        let pci = full.public_channel_info.as_ref().unwrap();
        assert!(pci.local_channel_announcement_signature.is_some());
        assert!(pci.remote_channel_announcement_signature.is_some());
        assert!(pci.remote_channel_announcement_nonce.is_some());
        assert!(pci.channel_announcement.is_some());
        assert!(pci.channel_update.is_some());

        // --- ChannelAnnouncement nested Options ---
        let ca = pci.channel_announcement.as_ref().unwrap();
        assert!(ca.node1_signature.is_some());
        assert!(ca.node2_signature.is_some());
        assert!(ca.ckb_signature.is_some());
        assert!(ca.udt_type_script.is_some());

        // --- ChannelUpdate nested Options ---
        let cu = pci.channel_update.as_ref().unwrap();
        assert!(cu.signature.is_some());

        // --- ShutdownInfo nested Options ---
        assert!(full
            .local_shutdown_info
            .as_ref()
            .unwrap()
            .signature
            .is_some());
        assert!(full
            .remote_shutdown_info
            .as_ref()
            .unwrap()
            .signature
            .is_some());

        // --- TlcState non-empty ---
        assert!(!full.tlc_state.offered_tlcs.tlcs.is_empty());
        assert!(!full.tlc_state.received_tlcs.tlcs.is_empty());

        // --- TlcInfo nested Options (offered) ---
        let offered = &full.tlc_state.offered_tlcs.tlcs[0];
        assert!(offered.total_amount.is_some());
        assert!(offered.payment_secret.is_some());
        assert!(offered.attempt_id.is_some());
        assert!(offered.onion_packet.is_some());
        assert!(offered.removed_reason.is_some());
        assert!(offered.forwarding_tlc.is_some());
        assert!(offered.removed_confirmed_at.is_some());

        // --- TlcInfo nested Options (received) ---
        let received = &full.tlc_state.received_tlcs.tlcs[0];
        assert!(received.total_amount.is_some());
        assert!(received.payment_secret.is_some());
        assert!(received.attempt_id.is_some());
        assert!(received.onion_packet.is_some());
        assert!(received.removed_reason.is_some());
        assert!(received.forwarding_tlc.is_some());
        assert!(received.removed_confirmed_at.is_some());

        // --- Collections non-empty ---
        assert!(!full.retryable_tlc_operations.is_empty());
        assert!(!full.waiting_forward_tlc_tasks.is_empty());
        assert!(!full.remote_commitment_points.is_empty());
    }
}
