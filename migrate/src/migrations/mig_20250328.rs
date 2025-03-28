use fiber::{store::migration::Migration, Error};
use fiber_v042::ckb::config::UdtDep;
use indicatif::ProgressBar;
use rocksdb::ops::Iterate;
use rocksdb::ops::Put;
use rocksdb::DB;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250328175130";

use crate::util::convert;
pub use fiber_v041::ckb::config::UdtArgInfo as UdtArgInfoV041;
pub use fiber_v041::ckb::config::UdtCellDep as UdtCellDepV041;
pub use fiber_v041::ckb::config::UdtCfgInfos as UdtCfgInfosV041;
pub use fiber_v041::fiber::types::BroadcastMessage as BroadcastMessageV041;
pub use fiber_v041::fiber::types::NodeAnnouncement as NodeAnnouncementV041;
pub use fiber_v042::ckb::config::UdtArgInfo as UdtArgInfoV042;
pub use fiber_v042::ckb::config::UdtCellDep as UdtCellDepV042;
pub use fiber_v042::ckb::config::UdtCfgInfos as UdtCfgInfosV042;
pub use fiber_v042::fiber::types::BroadcastMessage as BroadcastMessageV042;
pub use fiber_v042::fiber::types::NodeAnnouncement as NodeAnnouncementV042;
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

fn convert_udt_cfg_infos(old: UdtCfgInfosV041) -> UdtCfgInfosV042 {
    let mut udt_cfg_infos = Vec::new();
    for udt_cfg_info in old.0 {
        let cell_deps = udt_cfg_info
            .cell_deps
            .iter()
            .map(|cell_dep| {
                UdtDep::CellDep(UdtCellDepV042 {
                    tx_hash: convert(cell_dep.tx_hash.clone()),
                    index: cell_dep.index,
                    dep_type: cell_dep.dep_type,
                })
            })
            .collect();
        let udt_arg_info = UdtArgInfoV042 {
            name: udt_cfg_info.name,
            script: convert(udt_cfg_info.script),
            auto_accept_amount: udt_cfg_info.auto_accept_amount,
            cell_deps,
        };
        udt_cfg_infos.push(udt_arg_info);
    }
    UdtCfgInfosV042(udt_cfg_infos)
}

fn convert_node_announcement(old: NodeAnnouncementV041) -> NodeAnnouncementV042 {
    NodeAnnouncementV042 {
        signature: convert(old.signature),
        features: convert(old.features),
        timestamp: old.timestamp,
        node_id: convert(old.node_id),
        node_name: convert(old.node_name),
        addresses: old.addresses,
        chain_hash: convert(old.chain_hash),
        auto_accept_min_ckb_funding_amount: old.auto_accept_min_ckb_funding_amount,
        udt_cfg_infos: convert_udt_cfg_infos(old.udt_cfg_infos),
    }
}

impl Migration for MigrationObj {
    fn migrate(
        &self,
        db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error> {
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        const BROADCAST_MESSAGE_PREFIX: u8 = 96;
        let prefix = vec![BROADCAST_MESSAGE_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            if let Ok(_) = bincode::deserialize::<BroadcastMessageV042>(&v) {
                // if we can deserialize the data correctly with new version, just skip it.
                continue;
            }

            let old_broadcast_message: BroadcastMessageV041 =
                bincode::deserialize(&v).expect("deserialize to old broadcast message");

            let new_broadcast_message = match old_broadcast_message {
                BroadcastMessageV041::NodeAnnouncement(old_node_announcement) => {
                    let new_node_announcement = convert_node_announcement(old_node_announcement);
                    BroadcastMessageV042::NodeAnnouncement(new_node_announcement)
                }
                BroadcastMessageV041::ChannelAnnouncement(_channel_announcement) => {
                    continue;
                }
                BroadcastMessageV041::ChannelUpdate(_channel_update) => {
                    continue;
                }
            };

            let new_broadcast_message_bytes = bincode::serialize(&new_broadcast_message)
                .expect("serialize to new broadcast message");

            db.put(k, new_broadcast_message_bytes)
                .expect("save new broadcast message");
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
