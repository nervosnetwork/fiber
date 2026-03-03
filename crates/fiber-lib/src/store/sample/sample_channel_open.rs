/// StoreSample implementation for `ChannelOpenRecord` (prefix 201).
use crate::fiber::channel::{ChannelOpenRecord, ChannelOpeningStatus};
use crate::store::schema::CHANNEL_OPEN_RECORD_PREFIX;

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

fn make_pubkey(seed: u64, index: u32) -> crate::fiber::types::Pubkey {
    deterministic_pubkey(seed, index)
}

fn sample_waiting_for_peer(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 0),
        pubkey: make_pubkey(seed, 1),
        is_acceptor: false,
        status: ChannelOpeningStatus::WaitingForPeer,
        funding_amount: 100_0000_0000,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000,
    }
}

fn sample_funding_tx_building(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 10),
        pubkey: make_pubkey(seed, 11),
        is_acceptor: false,
        status: ChannelOpeningStatus::FundingTxBuilding,
        funding_amount: 200_0000_0000,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 500,
    }
}

fn sample_funding_tx_broadcasted(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 20),
        pubkey: make_pubkey(seed, 21),
        is_acceptor: false,
        status: ChannelOpeningStatus::FundingTxBroadcasted,
        funding_amount: 300_0000_0000,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 1000,
    }
}

fn sample_channel_ready(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 30),
        pubkey: make_pubkey(seed, 31),
        is_acceptor: false,
        status: ChannelOpeningStatus::ChannelReady,
        funding_amount: 400_0000_0000,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 2000,
    }
}

fn sample_failed(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 40),
        pubkey: make_pubkey(seed, 41),
        is_acceptor: false,
        status: ChannelOpeningStatus::Failed,
        funding_amount: 100_0000_0000,
        failure_detail: Some("Peer disconnected during channel opening".to_string()),
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 500,
    }
}

fn sample_inbound_waiting(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 50),
        pubkey: make_pubkey(seed, 51),
        is_acceptor: true,
        status: ChannelOpeningStatus::WaitingForPeer,
        funding_amount: 100_0000_0000,
        failure_detail: None,
        created_at: seed * 1000,
        last_updated_at: seed * 1000,
    }
}

fn sample_inbound_failed(seed: u64) -> ChannelOpenRecord {
    ChannelOpenRecord {
        channel_id: deterministic_hash256(seed, 60),
        pubkey: make_pubkey(seed, 61),
        is_acceptor: true,
        status: ChannelOpeningStatus::Failed,
        funding_amount: 100_0000_0000,
        failure_detail: Some("Peer disconnected during channel opening".to_string()),
        created_at: seed * 1000,
        last_updated_at: seed * 1000 + 300,
    }
}
