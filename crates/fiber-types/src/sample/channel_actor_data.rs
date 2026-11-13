/// StoreSample implementation for `ChannelActorData`.
use std::collections::{HashMap, VecDeque};

use crate::channel::{
    ChannelActorData, ChannelBasePublicKeys, ChannelConnectivityState, ChannelConstraints,
    ChannelState, ChannelTlcInfo, CommitmentNumbers, InMemorySigner, PendingTlcs, TlcState,
};
use crate::crate_time::SystemTime;
use crate::schema::CHANNEL_ACTOR_STATE_PREFIX;

use super::{
    deterministic_hash, deterministic_hash256, deterministic_privkey, deterministic_pubkey,
    StoreSample,
};

impl StoreSample for ChannelActorData {
    const STORE_PREFIX: u8 = CHANNEL_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "ChannelActorData";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_v2(seed)]
    }
}

/// A valid V2 bootstrap session for storage codec checks.
fn sample_v2(seed: u64) -> ChannelActorData {
    let signer = deterministic_signer(seed, 0);
    let local_channel_public_keys = channel_pubkeys_from_signer(&signer);
    let id = deterministic_hash256(seed, 1561);
    let mut own = crate::SigningNonceV2 {
        seed: deterministic_hash(seed, 1561),
        channel_id: id,
        owner: signer.funding_key.pubkey(),
        number: 1,
        purpose: crate::NoncePurposeV2::Commitment,
        public_nonce: deterministic_pub_nonce(seed, 1561),
        context: None,
        signature: None,
    };
    own.public_nonce =
        crate::channel_v2_validation::secret_nonce_v2(&own, &signer.funding_key).public_nonce();
    let session = crate::ChannelSessionV2 {
        marker: 0x56320001,
        own,
        own_retired: vec![],
        remote_nonce: None,
        remote_number: 1,
        bootstrap_remote_nonce: None,
        remote_ready: None,
        outgoing: None,
        incoming: None,
        pending_incoming: None,
        pending_ack: None,
        last_ack: None,
        revocation_effect: None,
        bootstrap_settlement: None,
        remove_effects: vec![],
        closing: None,
    };

    ChannelActorData {
        state: ChannelState::NegotiatingFunding(crate::NegotiatingFundingFlags::empty()),
        public_channel_info: None,
        local_tlc_info: ChannelTlcInfo::default(),
        remote_tlc_info: None,
        local_pubkey: deterministic_pubkey(seed, 10),
        remote_pubkey: deterministic_pubkey(seed, 11),
        id,
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
        latest_commitment_transaction: None,
        remote_commitment_points: vec![],
        remote_channel_public_keys: None,
        local_shutdown_info: None,
        remote_shutdown_info: None,
        shutdown_transaction_hash: None,
        reestablishing: false,
        connectivity_state: ChannelConnectivityState::Online,
        pending_replay_updates: vec![],
        last_was_revoke: false,
        external_funding: None,
        channel_features: Default::default(),
        session,
        created_at: SystemTime::UNIX_EPOCH,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_actor_data_samples_roundtrip() {
        ChannelActorData::verify_samples_roundtrip(42);
    }
}
