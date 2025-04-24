use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250227103033";

pub use fiber_v031::fiber::channel::{
    RevocationData as RevocationDataV031, SettlementData as SettlementDataV031,
};
pub use fiber_v031::watchtower::ChannelData as ChannelDataV031;

use crate::util::convert;
pub use fiber_v040::fiber::channel::SettlementData as SettlementDataV040;
pub use fiber_v040::watchtower::ChannelData as ChannelDataV040;

pub struct MigrationObj {
    version: String,
}

impl MigrationObj {
    pub fn new() -> Self {
        Self {
            version: MIGRATION_DB_VERSION.to_string(),
        }
    }
}

fn convert_channel_data(old: ChannelDataV031) -> ChannelDataV040 {
    ChannelDataV040 {
        channel_id: convert(old.channel_id),
        funding_tx_lock: old.funding_tx_lock,
        remote_settlement_data: convert_settlement_data(old.remote_settlement_data),
        local_settlement_data: old.local_settlement_data.map(convert_settlement_data),
        revocation_data: convert(old.revocation_data),
    }
}

fn convert_settlement_data(old: SettlementDataV031) -> SettlementDataV040 {
    // SettlementData adds a new `tlcs` field, just convert it with an empty Vec
    SettlementDataV040 {
        x_only_aggregated_pubkey: old.x_only_aggregated_pubkey,
        aggregated_signature: old.aggregated_signature,
        to_local_output: old.to_local_output,
        to_local_output_data: old.to_local_output_data,
        to_remote_output: old.to_remote_output,
        to_remote_output_data: old.to_remote_output_data,
        tlcs: vec![],
    }
}

impl Migration for MigrationObj {
    fn migrate<'a>(
        &self,
        db: &'a fiber::store::Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a fiber::store::Store, Error> {
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;
        let prefix = vec![WATCHTOWER_CHANNEL_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.clone().as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            let old_channel_data: ChannelDataV031 =
                bincode::deserialize(&v).expect("deserialize to old channel data");

            let new_channel_data = convert_channel_data(old_channel_data);
            let new_channel_data_bytes =
                bincode::serialize(&new_channel_data).expect("serialize to new channel data");

            db.put(k, new_channel_data_bytes);
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
