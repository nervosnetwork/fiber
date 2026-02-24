/// StoreSample implementation for `ChannelOpenRecord` (prefix 201).
use crate::fiber::channel::{ChannelOpenRecord, ChannelOpeningStatus};
use crate::store::schema::CHANNEL_OPEN_RECORD_PREFIX;
use tentacle::secio::PeerId;

use super::{deterministic_hash256, deterministic_pubkey, StoreSample};

impl StoreSample for ChannelOpenRecord {
    const STORE_PREFIX: u8 = CHANNEL_OPEN_RECORD_PREFIX;
    const TYPE_NAME: &'static str = "ChannelOpenRecord";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            sample_waiting_for_peer(seed),
            sample_funding_tx_building(seed),
            sample_funding_tx_broadcasted(seed),
            sample_channel_ready(seed),
            sample_failed(seed),
            sample_inbound_waiting(seed),
            sample_inbound_failed(seed),
        ]
    }
}

fn make_peer_id(seed: u64, index: u32) -> PeerId {
    let pubkey = deterministic_pubkey(seed, index);
    let pk_bytes = pubkey.serialize();
    let tentacle_pk = tentacle::secio::PublicKey::from_raw_key(pk_bytes.to_vec());
    PeerId::from_public_key(&tentacle_pk)
}

fn sample_waiting_for_peer(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 0),
        peer_id: make_peer_id(seed, 1),
        is_acceptor: false,
        status: ChannelOpeningStatus::WaitingForPeer,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000,
    }
}

fn sample_funding_tx_building(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 10),
        peer_id: make_peer_id(seed, 11),
        is_acceptor: false,
        status: ChannelOpeningStatus::FundingTxBuilding,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 500,
    }
}

fn sample_funding_tx_broadcasted(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 20),
        peer_id: make_peer_id(seed, 21),
        is_acceptor: false,
        status: ChannelOpeningStatus::FundingTxBroadcasted,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 1000,
    }
}

fn sample_channel_ready(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 30),
        peer_id: make_peer_id(seed, 31),
        is_acceptor: false,
        status: ChannelOpeningStatus::ChannelReady,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 2000,
    }
}

fn sample_failed(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 40),
        peer_id: make_peer_id(seed, 41),
        is_acceptor: false,
        status: ChannelOpeningStatus::Failed,
        failure_detail: Some("Peer disconnected during channel opening".to_string()),
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 500,
    }
}

fn sample_inbound_waiting(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 50),
        peer_id: make_peer_id(seed, 51),
        is_acceptor: true,
        status: ChannelOpeningStatus::WaitingForPeer,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000,
    }
}

fn sample_inbound_failed(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 60),
        peer_id: make_peer_id(seed, 61),
        is_acceptor: true,
        status: ChannelOpeningStatus::Failed,
        failure_detail: Some("Peer disconnected during channel opening".to_string()),
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 300,
    }
}
