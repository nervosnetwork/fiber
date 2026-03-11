use crate::channel::TLCId;
use crate::invoice::HashAlgorithm;
use crate::schema::WATCHTOWER_CHANNEL_PREFIX;
use crate::watchtower::{ChannelData, SettlementData, SettlementTlc};

use super::{deterministic_hash256, deterministic_privkey, deterministic_pubkey, StoreSample};

impl StoreSample for ChannelData {
    const STORE_PREFIX: u8 = WATCHTOWER_CHANNEL_PREFIX;
    const TYPE_NAME: &'static str = "ChannelData";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            // Minimal channel data
            ChannelData {
                channel_id: deterministic_hash256(seed, 0),
                funding_udt_type_script: None,
                local_settlement_key: deterministic_privkey(seed, 1),
                remote_settlement_key: deterministic_pubkey(seed, 2),
                local_funding_pubkey: deterministic_pubkey(seed, 3),
                remote_funding_pubkey: deterministic_pubkey(seed, 4),
                remote_settlement_data: SettlementData {
                    local_amount: 100_000_000_000,
                    remote_amount: 100_000_000_000,
                    tlcs: vec![],
                },
                pending_remote_settlement_data: SettlementData {
                    local_amount: 100_000_000_000,
                    remote_amount: 100_000_000_000,
                    tlcs: vec![],
                },
                local_settlement_data: SettlementData {
                    local_amount: 100_000_000_000,
                    remote_amount: 100_000_000_000,
                    tlcs: vec![],
                },
                revocation_data: None,
            },
            // Channel with TLCs and revocation data
            ChannelData {
                channel_id: deterministic_hash256(seed, 10),
                funding_udt_type_script: None,
                local_settlement_key: deterministic_privkey(seed, 11),
                remote_settlement_key: deterministic_pubkey(seed, 12),
                local_funding_pubkey: deterministic_pubkey(seed, 13),
                remote_funding_pubkey: deterministic_pubkey(seed, 14),
                remote_settlement_data: SettlementData {
                    local_amount: 80_000_000_000,
                    remote_amount: 120_000_000_000,
                    tlcs: vec![SettlementTlc {
                        tlc_id: TLCId::Offered(0),
                        hash_algorithm: HashAlgorithm::CkbHash,
                        payment_amount: 10_000_000_000,
                        payment_hash: deterministic_hash256(seed, 20),
                        expiry: 1_704_070_800,
                        local_key: deterministic_privkey(seed, 21),
                        remote_key: deterministic_pubkey(seed, 22),
                    }],
                },
                pending_remote_settlement_data: SettlementData {
                    local_amount: 80_000_000_000,
                    remote_amount: 120_000_000_000,
                    tlcs: vec![],
                },
                local_settlement_data: SettlementData {
                    local_amount: 90_000_000_000,
                    remote_amount: 110_000_000_000,
                    tlcs: vec![],
                },
                revocation_data: None, // Skip RevocationData with CompactSignature for simplicity
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_data_samples_roundtrip() {
        ChannelData::verify_samples_roundtrip(42);
    }
}
