use crate::channel::{ChannelOpenRecord, ChannelOpeningStatus};
use crate::schema::CHANNEL_OPEN_RECORD_PREFIX;

use super::{deterministic_hash256, deterministic_pubkey, StoreSample};

impl StoreSample for ChannelOpenRecord {
    const STORE_PREFIX: u8 = CHANNEL_OPEN_RECORD_PREFIX;
    const TYPE_NAME: &'static str = "ChannelOpenRecord";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            // Outbound channel waiting for peer
            ChannelOpenRecord {
                channel_id: deterministic_hash256(seed, 0),
                pubkey: deterministic_pubkey(seed, 1),
                is_acceptor: false,
                status: ChannelOpeningStatus::WaitingForPeer,
                funding_amount: 100_000_000_000,
                failure_detail: None,
                created_at: 1_704_067_200_000,
                last_updated_at: 1_704_067_200_000,
            },
            // Funding transaction building
            ChannelOpenRecord {
                channel_id: deterministic_hash256(seed, 10),
                pubkey: deterministic_pubkey(seed, 11),
                is_acceptor: false,
                status: ChannelOpeningStatus::FundingTxBuilding,
                funding_amount: 200_000_000_000,
                failure_detail: None,
                created_at: 1_704_067_200_100,
                last_updated_at: 1_704_067_200_200,
            },
            // Funding transaction broadcasted
            ChannelOpenRecord {
                channel_id: deterministic_hash256(seed, 15),
                pubkey: deterministic_pubkey(seed, 16),
                is_acceptor: false,
                status: ChannelOpeningStatus::FundingTxBroadcasted,
                funding_amount: 150_000_000_000,
                failure_detail: None,
                created_at: 1_704_067_200_250,
                last_updated_at: 1_704_067_200_280,
            },
            // Failed channel with error
            ChannelOpenRecord {
                channel_id: deterministic_hash256(seed, 20),
                pubkey: deterministic_pubkey(seed, 21),
                is_acceptor: false,
                status: ChannelOpeningStatus::Failed,
                funding_amount: 50_000_000_000,
                failure_detail: Some("Funding timeout".to_string()),
                created_at: 1_704_067_200_300,
                last_updated_at: 1_704_067_200_400,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_open_record_samples_roundtrip() {
        ChannelOpenRecord::verify_samples_roundtrip(42);
    }
}
