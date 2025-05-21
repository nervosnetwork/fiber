use fiber::fiber::channel::ChannelActorState;
use fiber::fiber::graph::PaymentSession;
use fiber::fiber::network::PersistentNetworkActorState;
use fiber::fiber::types::BroadcastMessage;
use fiber::fiber::{network::PeerInfo, types::NodeAnnouncement};
use fiber::store::schema::*;
use fiber::store::store_impl::deserialize_from;
use rand::{distributions::Alphanumeric, Rng};
use rocksdb::{prelude::*, ColumnFamilyDescriptor, SecondaryDB, SecondaryOpenDescriptor};
use std::path::Path;
use std::sync::Arc;
use tentacle::secio::PeerId;

#[derive(Debug, Clone)]
pub struct SecondaryStore {
    pub(crate) db: Arc<SecondaryDB>,
}

impl SecondaryStore {
    pub fn new_secondary<P: AsRef<Path>>(primary_path: P) -> SecondaryStore {
        let cf_names: Vec<String> = vec![];
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_keep_log_file_num(1);
        let secondary_path = std::env::temp_dir().join(format!(
            "fiber-gui-store-{}",
            rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(8)
                .map(char::from)
                .collect::<String>()
        ));

        let cf_descriptors: Vec<_> = cf_names
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()))
            .collect();

        let descriptor = SecondaryOpenDescriptor::new(secondary_path.to_string_lossy().to_string());
        let inner = SecondaryDB::open_cf_descriptors_with_descriptor(
            &opts,
            primary_path.as_ref(),
            cf_descriptors,
            descriptor,
        )
        .expect("Failed to open SecondaryDB");
        SecondaryStore {
            db: Arc::new(inner),
        }
    }

    fn prefix_iterator<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.db
            .prefix_iterator(prefix)
            .take_while(move |(col_key, _)| col_key.starts_with(prefix))
    }

    fn get_network_actor_state(&self, id: &PeerId) -> Option<PersistentNetworkActorState> {
        let key = [&[PEER_ID_NETWORK_ACTOR_STATE_PREFIX], id.as_bytes()].concat();
        self.db
            .get(key)
            .map(|value| {
                deserialize_from(
                    value.as_ref().expect("secondary value"),
                    "PersistentNetworkActorState",
                )
            })
            .ok()
    }

    /// List all peers for the given local peer_id (usually self).
    /// Returns a Vec<PeerInfo> for all known peers of this node.
    pub fn list_peers(&self, local_peer_id: &PeerId) -> Vec<PeerInfo> {
        if let Some(state) = self.get_network_actor_state(local_peer_id) {
            // For each peer_pubkey_map entry, build PeerInfo
            state
                .peer_pubkey_map
                .iter()
                .map(|(peer_id, pubkey)| {
                    let addresses = state.get_peer_addresses(peer_id);
                    PeerInfo {
                        pubkey: *pubkey,
                        peer_id: peer_id.clone(),
                        addresses,
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// List all known nodes (from PersistentNetworkActorState in the store).
    /// Returns a Vec<PeerInfo> for all known nodes.
    pub fn list_network_nodes(&self) -> Vec<NodeAnnouncement> {
        let prefix = [BROADCAST_MESSAGE_PREFIX];
        let mut result = Vec::new();
        for (_key, value) in self.prefix_iterator(&prefix) {
            let broadcast_message: BroadcastMessage =
                deserialize_from(value.as_ref(), "BroadcastMessage");
            if let BroadcastMessage::NodeAnnouncement(node) = broadcast_message {
                result.push(node);
            }
        }
        result.reverse();
        result
    }

    pub fn list_payment_sessions(&self) -> Vec<PaymentSession> {
        let prefix = [PAYMENT_SESSION_PREFIX];
        let mut result = Vec::new();
        for (_key, value) in self.prefix_iterator(&prefix) {
            let session: PaymentSession = deserialize_from(value.as_ref(), "PaymentSession");
            result.push(session);
        }
        result.reverse();
        result
    }

    pub fn list_direct_channels(&self) -> Vec<ChannelActorState> {
        let prefix = [CHANNEL_ACTOR_STATE_PREFIX];
        let mut result = Vec::new();
        for (_key, value) in self.prefix_iterator(&prefix) {
            let channel_actor_state: ChannelActorState =
                deserialize_from(value.as_ref(), "ChannelActorState");
            result.push(channel_actor_state);
        }
        result.reverse();
        result
    }
}
