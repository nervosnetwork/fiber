#[cfg(feature = "watchtower")]
use ckb_types::packed::Script;

use crate::store::store_trait::{FiberStore, PrefixIterOptions};
use fiber_store::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use fiber_store::iterator::{IteratorDirection, KVPair};
use fiber_store::StoreError;

use std::path::Path;
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use crate::cch::{CchOrderStore, CchStoreError};
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::onchain_tlc_reconcile::StoredOnChainTlcSettlement;
#[cfg(feature = "watchtower")]
use crate::fiber::onchain_tlc_reconcile::{LegacyOnChainTlcSettlement, OnChainTlcSettlement};
use crate::fiber::types::HoldTlc;
use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
    LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
};
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
use crate::{
    fiber::{
        channel::{ChannelActorState, ChannelActorStateStore, ChannelOpenRecordStore, CommitDiff},
        graph::NetworkGraphStateStore,
        network::NetworkActorStateStore,
        payment::PaymentSessionExt,
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore, PreimageStore},
};
use ckb_types::packed::OutPoint;
use ckb_types::prelude::Entity;
use fiber_store::db_migrate::DbMigrate;
use fiber_store::migration::{
    MigrateConfirmFn, MigrateProgressFn, INIT_DB_VERSION, MIGRATION_VERSION_KEY,
};
use fiber_types::schema::*;
use fiber_types::{
    Attempt, AttemptStatus, BroadcastMessage, BroadcastMessageID, ChannelOpenRecord, ChannelState,
    Cursor, Direction, Hash256, LiquidityAsset, LiquiditySwapState, PaymentCustomRecords,
    PaymentSession, PaymentStatus, PersistentNetworkActorState, Pubkey, TLCId, TimedResult,
    CURSOR_SIZE,
};
#[cfg(not(target_arch = "wasm32"))]
use fiber_types::{CchOrder, CchReceiveBtcOrderCreation, CchSendBtcOrderCreation};
#[cfg(feature = "watchtower")]
use fiber_types::{ChannelData, NodeId, Privkey, RevocationData, SettlementData};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::info;
#[cfg(all(feature = "watchtower", not(any(target_arch = "wasm32", test))))]
use tracing::warn;

/// Wrapper around `fiber_store::Store` that embeds an optional watcher callback.
///
/// The watcher is invoked after specific write operations (invoice insert/update,
/// preimage insert, payment session insert) to notify interested components
/// (e.g. the CCH subsystem) of store changes.
///
/// All production code accesses the store through domain traits
/// (`InvoiceStore`, `PreimageStore`, etc.), never through the concrete type.
#[derive(Clone)]
pub struct Store {
    inner: fiber_store::Store,
    watcher: Option<Arc<dyn Fn(StoreChange) + Send + Sync>>,
    #[cfg(feature = "watchtower")]
    watchtower_write_locks: Arc<parking_lot::Mutex<HashMap<NodeId, Arc<parking_lot::Mutex<()>>>>>,
    #[cfg(feature = "watchtower")]
    onchain_tlc_settlement_write_lock: Arc<parking_lot::Mutex<()>>,
}

#[cfg(feature = "watchtower")]
#[derive(Clone, Copy)]
enum WatchtowerPreimageCleanupTarget<'a> {
    Exact(&'a Hash256),
    ExactSet(&'a HashSet<Hash256>),
}

#[cfg(feature = "watchtower")]
impl WatchtowerPreimageCleanupTarget<'_> {
    fn matches(self, payment_hash: &Hash256) -> bool {
        match self {
            WatchtowerPreimageCleanupTarget::Exact(target) => payment_hash == target,
            WatchtowerPreimageCleanupTarget::ExactSet(targets) => targets.contains(payment_hash),
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("inner", &self.inner)
            .field("watcher", &self.watcher.as_ref().map(|_| "..."))
            .finish()
    }
}

impl Store {
    /// Set a watcher callback that will be invoked on relevant store changes.
    pub fn set_watcher(&mut self, watcher: Arc<dyn Fn(StoreChange) + Send + Sync>) {
        self.watcher = Some(watcher);
    }

    fn notify(&self, change: StoreChange) {
        if let Some(ref watcher) = self.watcher {
            watcher(change);
        }
    }

    fn channel_owns_attempt(&self, channel_outpoint: &OutPoint, attempt: &Attempt) -> bool {
        self.get_channel_state_by_outpoint(channel_outpoint)
            .is_some_and(|channel_state| {
                channel_state.owns_payment_attempt(attempt.payment_hash, attempt.id)
            })
    }

    #[cfg(feature = "watchtower")]
    fn watchtower_write_lock(&self, node_id: &NodeId) -> Arc<parking_lot::Mutex<()>> {
        self.watchtower_write_locks
            .lock()
            .entry(node_id.clone())
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(())))
            .clone()
    }

    fn liquidity_swap_key(swap_id: &Hash256) -> Vec<u8> {
        [&[LIQUIDITY_SWAP_PREFIX], swap_id.as_ref()].concat()
    }

    fn liquidity_swap_state_index_key(state: LiquiditySwapState, swap_id: &Hash256) -> Vec<u8> {
        [
            &[LIQUIDITY_SWAP_STATE_PREFIX],
            &[liquidity_state_key(state)],
            swap_id.as_ref(),
        ]
        .concat()
    }

    fn parse_liquidity_swap_id_from_index(key: &[u8]) -> Option<Hash256> {
        let offset = key.len().checked_sub(32)?;
        let bytes: [u8; 32] = key.get(offset..)?.try_into().ok()?;
        Some(bytes.into())
    }
}

impl StorageBackend for Store {
    type Batch = <fiber_store::Store as StorageBackend>::Batch;

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.inner.put(key, value)
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.inner.delete(key)
    }

    fn batch(&self) -> Self::Batch {
        self.inner.batch()
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        self.inner
            .collect_iterator(start, direction, take_while_fn, limit)
    }

    fn backup(&self, path: &Path) -> Result<(), StoreError> {
        self.inner.backup(path)
    }

    fn restore(&self, restore_path: &Path, db_path: &Path) -> Result<(), StoreError> {
        self.inner.restore(restore_path, db_path)
    }
}

#[derive(Copy, Clone)]
enum ChannelTimestamp {
    ChannelAnnouncement(),
    ChannelUpdateOfNode1(),
    ChannelUpdateOfNode2(),
}

pub(crate) fn serialize_to_vec<T: ?Sized + Serialize>(value: &T, field_name: &str) -> Vec<u8> {
    bincode::serialize(value)
        .unwrap_or_else(|e| panic!("serialization of {} failed: {}", field_name, e))
}

pub(crate) fn deserialize_from<'a, T>(slice: &'a [u8], field_name: &str) -> T
where
    T: serde::Deserialize<'a>,
{
    bincode::deserialize(slice)
        .unwrap_or_else(|e| panic!("deserialization of {} failed: {}", field_name, e))
}

/// Open a store at `path`, running auto-migration with auto-confirm.
/// Use this when no user interaction is needed (e.g. tests, simple setups).
pub fn open_store<P: AsRef<Path>>(path: P) -> Result<Store, String> {
    open_store_with_migration(path, Box::new(|_| true), Box::new(|_| {}))
}

/// Open a store at `path`, running auto-migration with custom confirm/progress callbacks.
/// Use this when user interaction is required (e.g. CLI, WASM).
pub fn open_store_with_migration<P: AsRef<Path>>(
    path: P,
    confirm_fn: MigrateConfirmFn,
    progress_fn: MigrateProgressFn,
) -> Result<Store, String> {
    let db = fiber_store::Store::open_db(path.as_ref())?;
    run_auto_migrate(&db, confirm_fn, progress_fn)?;
    Ok(Store {
        inner: db,
        watcher: None,
        #[cfg(feature = "watchtower")]
        watchtower_write_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        #[cfg(feature = "watchtower")]
        onchain_tlc_settlement_write_lock: Arc::new(parking_lot::Mutex::new(())),
    })
}

fn run_auto_migrate(
    db: &fiber_store::Store,
    confirm_fn: MigrateConfirmFn,
    progress_fn: MigrateProgressFn,
) -> Result<(), String> {
    let mut migrate = DbMigrate::new();
    fiber_store::migrations::register_all_migrations(&mut migrate);
    migrate
        .auto_migrate(db, confirm_fn, progress_fn)
        .map_err(|e| e.to_string())
}

pub fn check_validate<P: AsRef<Path>>(path: P) -> Result<(), String> {
    let db = fiber_store::Store::open_db(path.as_ref())?;
    let store = Store {
        inner: db,
        watcher: None,
        #[cfg(feature = "watchtower")]
        watchtower_write_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        #[cfg(feature = "watchtower")]
        onchain_tlc_settlement_write_lock: Arc::new(parking_lot::Mutex::new(())),
    };
    let mut errors = HashSet::new();

    fn check_deserialization<T: serde::de::DeserializeOwned>(
        value: &[u8],
        prefix_name: &str,
        errors: &mut HashSet<String>,
    ) {
        if let Err(e) = bincode::deserialize::<T>(value) {
            errors.insert(format!("Failed to deserialize {}: {:?}", prefix_name, e));
        }
    }

    for (key, value) in store.prefix_iterator([]) {
        if key.is_empty() {
            errors.insert("Encountered empty key".to_string());
            continue;
        }

        match key[0] {
            CHANNEL_ACTOR_STATE_PREFIX => {
                check_deserialization::<ChannelActorState>(
                    &value,
                    "CHANNEL_ACTOR_STATE_PREFIX",
                    &mut errors,
                );
            }
            PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX => {
                check_deserialization::<PersistentNetworkActorState>(
                    &value,
                    "PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX",
                    &mut errors,
                );
            }
            CKB_INVOICE_PREFIX => {
                check_deserialization::<CkbInvoice>(&value, "CKB_INVOICE_PREFIX", &mut errors);
            }
            PREIMAGE_PREFIX => {
                check_deserialization::<Hash256>(&value, "PREIMAGE_PREFIX", &mut errors);
            }
            CKB_INVOICE_STATUS_PREFIX => {
                check_deserialization::<CkbInvoiceStatus>(
                    &value,
                    "CKB_INVOICE_STATUS_PREFIX",
                    &mut errors,
                );
            }
            PUBKEY_CHANNEL_ID_PREFIX => {}
            CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX => {
                check_deserialization::<Hash256>(
                    &value,
                    "CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX",
                    &mut errors,
                );
            }
            BROADCAST_MESSAGE_PREFIX => {
                check_deserialization::<BroadcastMessage>(
                    &value,
                    "BROADCAST_MESSAGE_PREFIX",
                    &mut errors,
                );
            }
            BROADCAST_MESSAGE_TIMESTAMP_PREFIX => {}
            PAYMENT_SESSION_PREFIX => {
                check_deserialization::<PaymentSession>(
                    &value,
                    "PAYMENT_SESSION_PREFIX",
                    &mut errors,
                );
            }
            PAYMENT_HISTORY_TIMED_RESULT_PREFIX => {
                check_deserialization::<TimedResult>(
                    &value,
                    "PAYMENT_HISTORY_TIMED_RESULT_PREFIX",
                    &mut errors,
                );
            }
            PAYMENT_CUSTOM_RECORD_PREFIX => {
                check_deserialization::<PaymentCustomRecords>(
                    &value,
                    "PAYMENT_CUSTOM_RECORD_PREFIX",
                    &mut errors,
                );
            }
            #[cfg(not(target_arch = "wasm32"))]
            CCH_ORDER_PREFIX => {
                check_deserialization::<CchOrder>(&value, "CCH_ORDER_PREFIX", &mut errors);
            }
            #[cfg(not(target_arch = "wasm32"))]
            CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX => {
                check_deserialization::<CchReceiveBtcOrderCreation>(
                    &value,
                    "CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX",
                    &mut errors,
                );
            }
            #[cfg(not(target_arch = "wasm32"))]
            CCH_SEND_BTC_ORDER_CREATION_PREFIX => {
                check_deserialization::<CchSendBtcOrderCreation>(
                    &value,
                    "CCH_SEND_BTC_ORDER_CREATION_PREFIX",
                    &mut errors,
                );
            }
            #[cfg(feature = "watchtower")]
            WATCHTOWER_CHANNEL_PREFIX => {
                check_deserialization::<ChannelData>(
                    &value,
                    "WATCHTOWER_CHANNEL_PREFIX",
                    &mut errors,
                );
            }
            _ => {}
        }
    }

    let mut errors: Vec<String> = errors.into_iter().collect();
    {
        let mut migrate = DbMigrate::new();
        fiber_store::migrations::register_all_migrations(&mut migrate);
        let ordering = migrate.check(&store.inner);
        match ordering {
            std::cmp::Ordering::Greater => {
                let db_version = store
                    .inner
                    .get(MIGRATION_VERSION_KEY)
                    .map(|v| String::from_utf8(v).unwrap_or_default())
                    .unwrap_or_default();
                errors.push(format!(
                    "Database version ({}) is newer than the binary. \
                     Please upgrade fiber to a newer version.",
                    db_version
                ));
            }
            std::cmp::Ordering::Less => {
                let db_version = store
                    .inner
                    .get(MIGRATION_VERSION_KEY)
                    .map(|v| String::from_utf8(v).unwrap_or_default())
                    .unwrap_or_default();
                let mut msg = format!(
                    "Database version ({}) is older than the binary. Migration needed.",
                    db_version
                );
                // If the DB is older than the initial migration epoch, the user
                // must run the legacy fnn-migrate tool first.
                if db_version.as_str() < INIT_DB_VERSION {
                    msg.push_str(&format!(
                        " DB version {} predates the unified migration epoch ({}). \
                         Run fnn-migrate v0.8.x to upgrade before starting this binary.",
                        db_version, INIT_DB_VERSION
                    ));
                }
                errors.push(msg);
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    if errors.is_empty() {
        info!("All keys and values in the store are valid.");
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn parse_hold_tlc(key: &[u8], value: &[u8]) -> (Hash256, HoldTlc) {
    let payment_hash: [u8; 32] = key[1..33]
        .try_into()
        .expect("payment_hash should be 32 bytes");

    let channel_id: [u8; 32] = key[33..65]
        .try_into()
        .expect("channel_id should be 32 bytes");

    let tlc_id: u64 = u64::from_le_bytes(key[65..].try_into().expect("tlc_id should be 8 bytes"));

    let expired_at: u64 = deserialize_from(value, "HoldTlc");

    let hold_tlc = HoldTlc {
        channel_id: channel_id.into(),
        tlc_id,
        hold_expire_at: expired_at,
    };

    (payment_hash.into(), hold_tlc)
}

fn liquidity_state_key(state: LiquiditySwapState) -> u8 {
    match state {
        LiquiditySwapState::Created => 0,
        LiquiditySwapState::Quoted => 1,
        LiquiditySwapState::OnchainLockPending => 2,
        LiquiditySwapState::OnchainLocked => 3,
        LiquiditySwapState::PayoutPending => 4,
        LiquiditySwapState::PayoutLocked => 5,
        LiquiditySwapState::PaymentInFlight => 6,
        LiquiditySwapState::PaymentSettled => 7,
        LiquiditySwapState::ClaimPending => 8,
        LiquiditySwapState::RefundPending => 9,
        LiquiditySwapState::Success => 10,
        LiquiditySwapState::Failed => 11,
        LiquiditySwapState::Refunded => 12,
    }
}

pub enum KeyValue {
    ChannelActorState(Hash256, ChannelActorState),
    CkbInvoice(Hash256, CkbInvoice),
    Preimage(Hash256, Hash256),
    CkbInvoiceStatus(Hash256, CkbInvoiceStatus),
    PubkeyChannelId((Pubkey, Hash256), ChannelState),
    OutPointChannelId(OutPoint, Hash256),
    BroadcastMessageTimestamp(BroadcastMessageID, u64),
    BroadcastMessage(Cursor, BroadcastMessage),
    #[cfg(feature = "watchtower")]
    WatchtowerChannel(NodeId, Hash256, ChannelData),
    #[cfg(feature = "watchtower")]
    // Preimage record, the payment_hash in first position to allow fast retrieve
    WatchtowerPreimage(Hash256, NodeId, Hash256),
    #[cfg(feature = "watchtower")]
    // Index of NodeId -> Preimage PaymentHash, which allows we query preimages of a node
    WatchtowerNodePaymentHash(NodeId, Hash256),
    PaymentSession(Hash256, PaymentSession),
    PaymentHistoryTimedResult((OutPoint, Direction), TimedResult),
    PaymentCustomRecord(Hash256, PaymentCustomRecords),
    NetworkActorState(Pubkey, PersistentNetworkActorState),
    Attempt((Hash256, u64), Attempt),
    // Index for attempts by first hop channel outpoint
    // Key: (channel_outpoint, payment_hash, attempt_id), Value: ()
    AttemptChannelIndex((OutPoint, Hash256, u64)),
    HoldTlc((Hash256, Hash256, u64), u64),
    #[cfg(not(target_arch = "wasm32"))]
    CchOrder(Hash256, CchOrder),
    #[cfg(not(target_arch = "wasm32"))]
    CchReceiveBtcOrderCreation(Hash256, CchReceiveBtcOrderCreation),
    #[cfg(not(target_arch = "wasm32"))]
    CchSendBtcOrderCreation(Hash256, CchSendBtcOrderCreation),
    ChannelOpenRecord(Hash256, ChannelOpenRecord),
    LiquiditySwap(Hash256, LiquiditySwapRecord),
    LiquiditySwapStateIndex((LiquiditySwapState, Hash256)),
    LiquiditySwapAssetIndex((String, Hash256)),
    LiquidityAsset(String, LiquidityAsset),
}

/// Recorded store changes.
///
/// This is a subset of all `put_kv(KeyValue)` and `delete(&[u8])` changes. Only interested changes
/// are recorded and sent to watchers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StoreChange {
    PutPreimage {
        payment_hash: Hash256,
        payment_preimage: Hash256,
    },
    PutCkbInvoiceStatus {
        payment_hash: Hash256,
        invoice_status: CkbInvoiceStatus,
    },
    PutPaymentSession {
        payment_hash: Hash256,
        payment_session: PaymentSession,
        #[serde(default)]
        payment_preimage: Option<Hash256>,
    },
    PutAttempt {
        payment_hash: Hash256,
        attempt_status: AttemptStatus,
    },
}

pub trait StoreKeyValue {
    fn key(&self) -> Vec<u8>;
    fn value(&self) -> Vec<u8>;
}

impl StoreKeyValue for KeyValue {
    fn key(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(id, _) => {
                [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat()
            }
            KeyValue::CkbInvoice(id, _) => [&[CKB_INVOICE_PREFIX], id.as_ref()].concat(),
            KeyValue::Preimage(id, _) => [&[PREIMAGE_PREFIX], id.as_ref()].concat(),
            KeyValue::CkbInvoiceStatus(id, _) => {
                [&[CKB_INVOICE_STATUS_PREFIX], id.as_ref()].concat()
            }
            KeyValue::PubkeyChannelId((pubkey, channel_id), _) => {
                let pubkey_bytes = pubkey.serialize();
                [
                    &[PUBKEY_CHANNEL_ID_PREFIX][..],
                    &pubkey_bytes[..],
                    channel_id.as_ref(),
                ]
                .concat()
            }
            KeyValue::OutPointChannelId(outpoint, _) => {
                [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat()
            }
            KeyValue::PaymentSession(payment_hash, _) => {
                [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat()
            }
            KeyValue::Attempt((payment_hash, attempt_id), _) => [
                &[ATTEMPT_PREFIX],
                payment_hash.as_ref(),
                &attempt_id.to_le_bytes(),
            ]
            .concat(),
            KeyValue::AttemptChannelIndex((channel_outpoint, payment_hash, attempt_id)) => [
                &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                channel_outpoint.as_slice(),
                payment_hash.as_ref(),
                &attempt_id.to_le_bytes(),
            ]
            .concat(),
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerChannel(node_id, channel_id, _) => [
                &[WATCHTOWER_CHANNEL_PREFIX],
                node_id.as_ref(),
                channel_id.as_ref(),
            ]
            .concat(),

            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerPreimage(payment_hash, node_id, _) => [
                &[WATCHTOWER_PREIMAGE_PREFIX],
                payment_hash.as_ref(),
                node_id.as_ref(),
            ]
            .concat(),
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerNodePaymentHash(node_id, payment_hash) => [
                &[WATCHTOWER_NODE_PAYMENTHASH_PREFIX],
                node_id.as_ref(),
                payment_hash.as_ref(),
            ]
            .concat(),
            KeyValue::NetworkActorState(pubkey, _) => {
                let pubkey_bytes = pubkey.serialize();
                [&[PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX], &pubkey_bytes[..]].concat()
            }
            KeyValue::PaymentHistoryTimedResult((channel_outpoint, direction), _) => [
                &[PAYMENT_HISTORY_TIMED_RESULT_PREFIX],
                channel_outpoint.as_slice(),
                serialize_to_vec(direction, "Direction").as_slice(),
            ]
            .concat(),
            KeyValue::BroadcastMessageTimestamp(broadcast_message_id, _) => [
                &[BROADCAST_MESSAGE_TIMESTAMP_PREFIX],
                broadcast_message_id.to_bytes().as_slice(),
            ]
            .concat(),
            KeyValue::BroadcastMessage(cursor, _) => {
                [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat()
            }
            KeyValue::PaymentCustomRecord(payment_hash, _data) => {
                [&[PAYMENT_CUSTOM_RECORD_PREFIX], payment_hash.as_ref()].concat()
            }
            KeyValue::HoldTlc((payment_hash, channel_id, tlc_id), _hold_tlc) => [
                &[HOLD_TLC_PREFIX],
                payment_hash.as_ref(),
                channel_id.as_ref(),
                &tlc_id.to_le_bytes(),
            ]
            .concat(),
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchOrder(payment_hash, _data) => {
                [&[CCH_ORDER_PREFIX], payment_hash.as_ref()].concat()
            }
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchReceiveBtcOrderCreation(payment_hash, _data) => [
                &[CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX],
                payment_hash.as_ref(),
            ]
            .concat(),
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchSendBtcOrderCreation(payment_hash, _data) => {
                [&[CCH_SEND_BTC_ORDER_CREATION_PREFIX], payment_hash.as_ref()].concat()
            }
            KeyValue::ChannelOpenRecord(channel_id, _) => {
                [&[CHANNEL_OPEN_RECORD_PREFIX], channel_id.as_ref()].concat()
            }
            KeyValue::LiquiditySwap(swap_id, _) => {
                [&[LIQUIDITY_SWAP_PREFIX], swap_id.as_ref()].concat()
            }
            KeyValue::LiquiditySwapStateIndex((state, swap_id)) => [
                &[LIQUIDITY_SWAP_STATE_PREFIX],
                &[liquidity_state_key(*state)],
                swap_id.as_ref(),
            ]
            .concat(),
            KeyValue::LiquiditySwapAssetIndex((asset_id, swap_id)) => [
                &[LIQUIDITY_SWAP_ASSET_PREFIX],
                asset_id.as_bytes(),
                &[0],
                swap_id.as_ref(),
            ]
            .concat(),
            KeyValue::LiquidityAsset(asset_id, _) => {
                [&[LIQUIDITY_ASSET_PREFIX], asset_id.as_bytes()].concat()
            }
        }
    }

    fn value(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(_, state) => serialize_to_vec(state, "ChannelActorState"),
            KeyValue::CkbInvoice(_, invoice) => serialize_to_vec(invoice, "CkbInvoice"),
            KeyValue::Preimage(_, preimage) => serialize_to_vec(preimage, "Hash256"),
            KeyValue::CkbInvoiceStatus(_, status) => serialize_to_vec(status, "CkbInvoiceStatus"),
            KeyValue::PubkeyChannelId(_, state) => serialize_to_vec(state, "ChannelState"),
            KeyValue::OutPointChannelId(_, channel_id) => serialize_to_vec(channel_id, "ChannelId"),
            KeyValue::PaymentSession(_, payment_session) => {
                serialize_to_vec(payment_session, "PaymentSession")
            }
            KeyValue::Attempt(_, attempt) => serialize_to_vec(attempt, "Attempt"),
            KeyValue::AttemptChannelIndex(_) => vec![], // Index only, no value needed
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerChannel(_, _, channel_data) => {
                serialize_to_vec(channel_data, "ChannelData")
            }
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerPreimage(_, _, preimage) => serialize_to_vec(preimage, "Hash256"),
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerNodePaymentHash(..) => Vec::new(),
            KeyValue::NetworkActorState(_, persistent_network_actor_state) => serialize_to_vec(
                persistent_network_actor_state,
                "PersistentNetworkActorState",
            ),
            KeyValue::BroadcastMessageTimestamp(_, value) => value.to_be_bytes().into(),
            KeyValue::BroadcastMessage(_cursor, broadcast_message) => {
                serialize_to_vec(broadcast_message, "BroadcastMessage")
            }
            KeyValue::PaymentHistoryTimedResult(_, result) => {
                serialize_to_vec(result, "TimedResult")
            }
            KeyValue::PaymentCustomRecord(_, custom_records) => {
                serialize_to_vec(custom_records, "PaymentCustomRecord")
            }
            KeyValue::HoldTlc(_, expired_at) => serialize_to_vec(expired_at, "HoldTlc"),
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchOrder(_, cch_order) => serialize_to_vec(cch_order, "CchOrder"),
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchReceiveBtcOrderCreation(_, creation) => {
                serialize_to_vec(creation, "CchReceiveBtcOrderCreation")
            }
            #[cfg(not(target_arch = "wasm32"))]
            KeyValue::CchSendBtcOrderCreation(_, creation) => {
                serialize_to_vec(creation, "CchSendBtcOrderCreation")
            }
            KeyValue::ChannelOpenRecord(_, record) => serialize_to_vec(record, "ChannelOpenRecord"),
            KeyValue::LiquiditySwap(_, swap) => serialize_to_vec(swap, "LiquiditySwapRecord"),
            KeyValue::LiquiditySwapStateIndex(_) => Vec::new(),
            KeyValue::LiquiditySwapAssetIndex(_) => Vec::new(),
            KeyValue::LiquidityAsset(_, asset) => serialize_to_vec(asset, "LiquidityAsset"),
        }
    }
}

#[cfg(feature = "watchtower")]
impl Store {
    fn legacy_tlc_on_chain_settled_key(channel_id: &Hash256, payment_hash: &[u8; 20]) -> Vec<u8> {
        [
            &[WATCHTOWER_TLC_SETTLED_PREFIX],
            channel_id.as_ref(),
            payment_hash.as_ref(),
        ]
        .concat()
    }

    fn tlc_on_chain_settled_key(node_id: &NodeId, channel_id: &Hash256, tlc_id: TLCId) -> Vec<u8> {
        let (direction, id) = match tlc_id {
            TLCId::Offered(id) => (0u8, id),
            TLCId::Received(id) => (1u8, id),
        };
        [
            &[WATCHTOWER_TLC_SETTLED_PREFIX],
            channel_id.as_ref(),
            &[2u8, direction],
            &id.to_be_bytes(),
            node_id.as_ref(),
        ]
        .concat()
    }

    fn watchtower_preimage_key(node_id: &NodeId, payment_hash: &Hash256) -> Vec<u8> {
        [
            &[WATCHTOWER_PREIMAGE_PREFIX],
            payment_hash.as_ref(),
            node_id.as_ref(),
        ]
        .concat()
    }

    fn watchtower_node_payment_hash_key(node_id: &NodeId, payment_hash: &Hash256) -> Vec<u8> {
        [
            &[WATCHTOWER_NODE_PAYMENTHASH_PREFIX],
            node_id.as_ref(),
            payment_hash.as_ref(),
        ]
        .concat()
    }

    fn parse_watchtower_scoped_payment_hash_key(key: &[u8]) -> Option<(NodeId, Hash256)> {
        if key.len() < 1 + 32 {
            return None;
        }
        let payment_hash_offset = key.len() - 32;
        let payment_hash: [u8; 32] = key[payment_hash_offset..].try_into().ok()?;
        Some((
            NodeId::from_bytes(key[1..payment_hash_offset].to_vec()),
            payment_hash.into(),
        ))
    }

    fn parse_watchtower_channel_key(key: &[u8]) -> Option<(NodeId, Hash256)> {
        if key.len() < 1 + 32 {
            return None;
        }
        let channel_id_offset = key.len() - 32;
        let channel_id: [u8; 32] = key[channel_id_offset..].try_into().ok()?;
        Some((
            NodeId::from_bytes(key[1..channel_id_offset].to_vec()),
            channel_id.into(),
        ))
    }

    fn watch_channels_for_node(&self, node_id: &NodeId) -> Vec<ChannelData> {
        let prefix = [&[WATCHTOWER_CHANNEL_PREFIX], node_id.as_ref()].concat();
        self.collect_by_prefix(&prefix)
            .into_iter()
            .filter_map(|kv| {
                let (channel_node_id, _) = Self::parse_watchtower_channel_key(&kv.key)?;
                (channel_node_id == *node_id)
                    .then(|| deserialize_from(kv.value.as_ref(), "ChannelData"))
            })
            .collect()
    }

    fn watch_channel_needs_preimage(
        &self,
        node_id: &NodeId,
        channel_data: &ChannelData,
        payment_hash: &Hash256,
    ) -> bool {
        let snapshot_statuses = [
            (&channel_data.remote_settlement_data, true),
            (&channel_data.pending_remote_settlement_data, true),
            (&channel_data.local_settlement_data, false),
        ]
        .into_iter()
        .filter_map(|(settlement_data, for_remote)| {
            let mut has_matching_tlc = false;
            let mut has_exact_settlement = false;
            let mut has_unsettled_tlc = false;
            for tlc in settlement_data
                .tlcs
                .iter()
                .filter(|tlc| &tlc.payment_hash == payment_hash)
            {
                has_matching_tlc = true;
                let tlc_id = if for_remote {
                    tlc.tlc_id
                } else {
                    tlc.tlc_id.flip()
                };
                let is_exactly_settled = self
                    .get(Self::tlc_on_chain_settled_key(
                        node_id,
                        &channel_data.channel_id,
                        tlc_id,
                    ))
                    .map(|value| {
                        deserialize_from::<OnChainTlcSettlement>(
                            value.as_ref(),
                            "OnChainTlcSettlement",
                        )
                    })
                    .is_some_and(|settlement| {
                        settlement.payment_hash == tlc.payment_hash
                            && settlement.hash_algorithm == tlc.hash_algorithm
                    });
                has_exact_settlement |= is_exactly_settled;
                has_unsettled_tlc |= !is_exactly_settled;
            }
            has_matching_tlc.then_some((has_exact_settlement, has_unsettled_tlc))
        })
        .collect::<Vec<_>>();

        // ChannelData retains several candidate commitment snapshots. Once an exact settlement
        // exists, only snapshots containing exact evidence can represent the active on-chain
        // chain. Within those candidates every same-hash TLC must be settled before its shared
        // preimage can be removed. Prefix-keyed legacy records intentionally provide no evidence
        // here because they cannot identify a specific TLC.
        if snapshot_statuses
            .iter()
            .any(|(has_exact_settlement, _)| *has_exact_settlement)
        {
            snapshot_statuses
                .iter()
                .any(|(has_exact_settlement, has_unsettled_tlc)| {
                    *has_exact_settlement && *has_unsettled_tlc
                })
        } else {
            !snapshot_statuses.is_empty()
        }
    }

    fn watch_channel_payment_hashes(channel_data: &ChannelData) -> HashSet<Hash256> {
        [
            &channel_data.remote_settlement_data,
            &channel_data.pending_remote_settlement_data,
            &channel_data.local_settlement_data,
        ]
        .into_iter()
        .flat_map(|settlement_data| settlement_data.tlcs.iter().map(|tlc| tlc.payment_hash))
        .collect()
    }

    fn watch_preimage_in_use(&self, node_id: &NodeId, payment_hash: &Hash256) -> bool {
        self.watch_channels_for_node(node_id)
            .iter()
            .any(|channel_data| {
                self.watch_channel_needs_preimage(node_id, channel_data, payment_hash)
            })
    }

    fn watch_preimage_entries(&self, node_id: Option<&NodeId>) -> Vec<(NodeId, Hash256)> {
        let prefix = match node_id {
            Some(node_id) => [&[WATCHTOWER_NODE_PAYMENTHASH_PREFIX], node_id.as_ref()].concat(),
            None => vec![WATCHTOWER_NODE_PAYMENTHASH_PREFIX],
        };
        self.collect_by_prefix(&prefix)
            .into_iter()
            .filter_map(|kv| Self::parse_watchtower_scoped_payment_hash_key(&kv.key))
            .filter(|(preimage_node_id, _)| {
                node_id.is_none_or(|node_id| preimage_node_id == node_id)
            })
            .collect()
    }

    fn cleanup_unused_watch_preimages(
        &self,
        node_id: Option<&NodeId>,
        target: WatchtowerPreimageCleanupTarget<'_>,
    ) {
        match node_id {
            Some(node_id) => {
                self.cleanup_unused_watch_preimages_with_hook(node_id, target, || {});
            }
            None => {
                let node_ids: HashSet<_> = self
                    .watch_preimage_entries(None)
                    .into_iter()
                    .filter(|(_, payment_hash)| target.matches(payment_hash))
                    .map(|(node_id, _)| node_id)
                    .collect();
                for node_id in node_ids {
                    let lock = self.watchtower_write_lock(&node_id);
                    let _guard = lock.lock();
                    self.cleanup_unused_watch_preimages_locked(&node_id, target, || {});
                }
            }
        }
    }

    fn cleanup_unused_watch_preimages_with_hook(
        &self,
        node_id: &NodeId,
        target: WatchtowerPreimageCleanupTarget<'_>,
        before_commit: impl FnOnce(),
    ) {
        let lock = self.watchtower_write_lock(node_id);
        let _guard = lock.lock();
        self.cleanup_unused_watch_preimages_locked(node_id, target, before_commit);
    }

    fn cleanup_unused_watch_preimages_locked(
        &self,
        node_id: &NodeId,
        target: WatchtowerPreimageCleanupTarget<'_>,
        before_commit: impl FnOnce(),
    ) {
        let preimages = match target {
            WatchtowerPreimageCleanupTarget::Exact(payment_hash) => {
                if self
                    .get(Self::watchtower_node_payment_hash_key(
                        node_id,
                        payment_hash,
                    ))
                    .is_some()
                {
                    vec![(node_id.clone(), *payment_hash)]
                } else {
                    Vec::new()
                }
            }
            _ => self.watch_preimage_entries(Some(node_id)),
        };
        if preimages.is_empty() {
            return;
        }

        let mut batch = self.batch();
        let mut has_change = false;
        for (node_id, payment_hash) in preimages {
            if !target.matches(&payment_hash) {
                continue;
            }

            if !self.watch_preimage_in_use(&node_id, &payment_hash) {
                batch.delete(Self::watchtower_preimage_key(&node_id, &payment_hash));
                batch.delete(Self::watchtower_node_payment_hash_key(
                    &node_id,
                    &payment_hash,
                ));
                has_change = true;
            }
        }
        if has_change {
            before_commit();
            batch.commit();
        }
    }
}

impl NetworkActorStateStore for Store {
    fn get_network_actor_state(&self, id: &Pubkey) -> Option<PersistentNetworkActorState> {
        let key = [
            &[PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX],
            &id.serialize()[..],
        ]
        .concat();
        self.get(key)
            .map(|value| deserialize_from(value.as_ref(), "PersistentNetworkActorState"))
    }

    fn insert_network_actor_state(&self, id: &Pubkey, state: PersistentNetworkActorState) {
        let mut batch = self.batch();
        let kv = KeyValue::NetworkActorState(*id, state);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }
}

impl ChannelActorStateStore for Store {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        let key = [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "ChannelActorState"))
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        let mut batch = self.batch();

        let kv = KeyValue::PubkeyChannelId((state.get_remote_pubkey(), state.id), state.state);
        batch.put(kv.key(), kv.value());
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let kv = KeyValue::OutPointChannelId(outpoint, state.id);
            batch.put(kv.key(), kv.value());
        }
        let kv = KeyValue::ChannelActorState(state.id, state);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn insert_channel_actor_state_with_pending_commit_diff(
        &self,
        state: ChannelActorState,
        diff: &CommitDiff,
    ) {
        let channel_id = state.get_id();
        let mut batch = self.batch();

        let kv = KeyValue::PubkeyChannelId((state.get_remote_pubkey(), state.id), state.state);
        batch.put(kv.key(), kv.value());
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let kv = KeyValue::OutPointChannelId(outpoint, state.id);
            batch.put(kv.key(), kv.value());
        }
        let kv = KeyValue::ChannelActorState(state.id, state);
        batch.put(kv.key(), kv.value());

        let key = [&[PENDING_COMMIT_DIFF_PREFIX], channel_id.as_ref()].concat();
        batch.put(key, serialize_to_vec(diff, "CommitDiff"));
        batch.commit();
    }

    fn move_channel_actor_state(&self, old_id: &Hash256, state: ChannelActorState) {
        if old_id == &state.id {
            self.insert_channel_actor_state(state);
            return;
        }

        let old_state = self.get_channel_actor_state(old_id);
        let mut batch = self.batch();

        if let Some(old_state) = old_state {
            batch.delete([&[CHANNEL_ACTOR_STATE_PREFIX], old_id.as_ref()].concat());
            let remote_pubkey_bytes = old_state.get_remote_pubkey().serialize();
            batch.delete(
                [
                    &[PUBKEY_CHANNEL_ID_PREFIX][..],
                    &remote_pubkey_bytes[..],
                    old_id.as_ref(),
                ]
                .concat(),
            );
            if let Some(outpoint) = old_state.get_funding_transaction_outpoint() {
                batch.delete([&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat());
            }
        }

        let kv = KeyValue::PubkeyChannelId((state.get_remote_pubkey(), state.id), state.state);
        batch.put(kv.key(), kv.value());
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let kv = KeyValue::OutPointChannelId(outpoint, state.id);
            batch.put(kv.key(), kv.value());
        }
        let kv = KeyValue::ChannelActorState(state.id, state);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        if let Some(state) = self.get_channel_actor_state(id) {
            let mut batch = self.batch();
            batch.delete([&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat());
            let remote_pubkey_bytes = state.get_remote_pubkey().serialize();
            batch.delete(
                [
                    &[PUBKEY_CHANNEL_ID_PREFIX][..],
                    &remote_pubkey_bytes[..],
                    id.as_ref(),
                ]
                .concat(),
            );
            if let Some(outpoint) = state.get_funding_transaction_outpoint() {
                batch.delete([&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat());
            }
            batch.commit();
        }
    }

    fn get_channel_ids_by_pubkey(&self, pubkey: &Pubkey) -> Vec<Hash256> {
        let pubkey_bytes = pubkey.serialize();
        let prefix = [&[PUBKEY_CHANNEL_ID_PREFIX][..], &pubkey_bytes[..]].concat();
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                let channel_id: [u8; 32] = kv.key[prefix.len()..]
                    .try_into()
                    .expect("channel id should be 32 bytes");
                channel_id.into()
            })
            .collect()
    }

    fn get_channel_states(&self, pubkey: Option<Pubkey>) -> Vec<(Pubkey, Hash256, ChannelState)> {
        let prefix = match pubkey {
            Some(pubkey) => {
                let pubkey_bytes = pubkey.serialize();
                [&[PUBKEY_CHANNEL_ID_PREFIX][..], &pubkey_bytes[..]].concat()
            }
            None => vec![PUBKEY_CHANNEL_ID_PREFIX],
        };
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                let key_len = kv.key.len();
                let pubkey = Pubkey::from_slice(&kv.key[1..key_len - 32])
                    .expect("deserialize pubkey should be OK");
                let channel_id: [u8; 32] = kv.key[key_len - 32..]
                    .try_into()
                    .expect("channel id should be 32 bytes");
                let state = deserialize_from(kv.value.as_ref(), "ChannelState");
                (pubkey, channel_id.into(), state)
            })
            .collect()
    }

    fn get_channel_state_by_outpoint(&self, outpoint: &OutPoint) -> Option<ChannelActorState> {
        let key = [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat();
        self.get(key)
            .map(|channel_id| deserialize_from(channel_id.as_ref(), "Hash256"))
            .and_then(|channel_id: Hash256| self.get_channel_actor_state(&channel_id))
    }

    fn get_all_channel_states(&self) -> Vec<ChannelActorState> {
        let prefix = &[CHANNEL_ACTOR_STATE_PREFIX];
        self.collect_by_prefix(prefix)
            .into_iter()
            .map(|kv| deserialize_from(kv.value.as_ref(), "ChannelActorState"))
            .collect()
    }

    fn insert_payment_custom_records(
        &self,
        payment_hash: &Hash256,
        custom_records: PaymentCustomRecords,
    ) {
        let mut batch = self.batch();
        let kv = KeyValue::PaymentCustomRecord(*payment_hash, custom_records);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn get_payment_custom_records(&self, payment_hash: &Hash256) -> Option<PaymentCustomRecords> {
        let key = [&[PAYMENT_CUSTOM_RECORD_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "PaymentCustomRecord"))
    }

    fn insert_payment_hold_tlc(&self, payment_hash: Hash256, hold_tlc: HoldTlc) {
        let mut batch = self.batch();
        let kv = KeyValue::HoldTlc(
            (payment_hash, hold_tlc.channel_id, hold_tlc.tlc_id),
            hold_tlc.hold_expire_at,
        );
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn remove_payment_hold_tlc(&self, payment_hash: &Hash256, channel_id: &Hash256, tlc_id: u64) {
        let prefix = [
            &[HOLD_TLC_PREFIX],
            payment_hash.as_ref(),
            channel_id.as_ref(),
            &tlc_id.to_le_bytes(),
        ]
        .concat();
        let mut batch = self.batch();
        batch.delete(prefix);
        batch.commit();
    }

    fn get_payment_hold_tlcs(&self, payment_hash: Hash256) -> Vec<HoldTlc> {
        let prefix = [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat();
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                let (_, hold_tlc) = parse_hold_tlc(&kv.key, &kv.value);
                hold_tlc
            })
            .collect()
    }

    fn get_node_hold_tlcs(&self) -> HashMap<Hash256, Vec<HoldTlc>> {
        let prefix = [HOLD_TLC_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| parse_hold_tlc(&kv.key, &kv.value))
            .fold(
                HashMap::new(),
                |mut acc: HashMap<Hash256, Vec<HoldTlc>>, (payment_hash, hold_tlc)| {
                    acc.entry(payment_hash).or_default().push(hold_tlc);
                    acc
                },
            )
    }

    fn get_onchain_tlc_settlement(
        &self,
        channel_id: &Hash256,
        tlc_id: TLCId,
        payment_hash: &Hash256,
    ) -> Option<StoredOnChainTlcSettlement> {
        #[cfg(feature = "watchtower")]
        {
            WatchtowerStore::get_onchain_tlc_settlement(
                self,
                &NodeId::local(),
                channel_id,
                tlc_id,
                payment_hash,
            )
        }
        #[cfg(not(feature = "watchtower"))]
        {
            let _ = (channel_id, tlc_id, payment_hash);
            None
        }
    }

    fn store_pending_commit_diff(&self, channel_id: &Hash256, diff: &CommitDiff) {
        let key = [&[PENDING_COMMIT_DIFF_PREFIX], channel_id.as_ref()].concat();
        let value = serialize_to_vec(diff, "CommitDiff");
        self.put(key, value);
    }

    fn get_pending_commit_diff(&self, channel_id: &Hash256) -> Option<CommitDiff> {
        let key = [&[PENDING_COMMIT_DIFF_PREFIX], channel_id.as_ref()].concat();
        self.get(&key).map(|v| deserialize_from(&v, "CommitDiff"))
    }

    fn delete_pending_commit_diff(&self, channel_id: &Hash256) {
        let key = [&[PENDING_COMMIT_DIFF_PREFIX], channel_id.as_ref()].concat();
        self.delete(&key);
    }
}

impl ChannelOpenRecordStore for Store {
    fn get_channel_open_records(&self) -> Vec<ChannelOpenRecord> {
        let prefix = [CHANNEL_OPEN_RECORD_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| deserialize_from(kv.value.as_ref(), "ChannelOpenRecord"))
            .collect()
    }

    fn get_channel_open_record(&self, channel_id: &Hash256) -> Option<ChannelOpenRecord> {
        let key = [&[CHANNEL_OPEN_RECORD_PREFIX], channel_id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "ChannelOpenRecord"))
    }

    fn insert_channel_open_record(&self, record: ChannelOpenRecord) {
        let mut batch = self.batch();
        let kv = KeyValue::ChannelOpenRecord(record.channel_id, record);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn delete_channel_open_record(&self, channel_id: &Hash256) {
        let key = [&[CHANNEL_OPEN_RECORD_PREFIX], channel_id.as_ref()].concat();
        self.delete(key);
    }
}

impl LiquidityStore for Store {
    fn insert_liquidity_swap(&self, swap: LiquiditySwapRecord) -> Result<(), LiquidityStoreError> {
        if self.get_liquidity_swap(&swap.swap_id)?.is_some() {
            return Err(LiquidityStoreError::Backend(format!(
                "liquidity swap already exists: {:?}",
                swap.swap_id
            )));
        }

        let mut batch = self.batch();
        let primary = KeyValue::LiquiditySwap(swap.swap_id, swap.clone());
        let state_index = KeyValue::LiquiditySwapStateIndex((swap.state, swap.swap_id));
        let asset_index = KeyValue::LiquiditySwapAssetIndex((swap.asset_id.clone(), swap.swap_id));
        batch.put(primary.key(), primary.value());
        batch.put(state_index.key(), state_index.value());
        batch.put(asset_index.key(), asset_index.value());
        batch.commit();
        Ok(())
    }

    fn get_liquidity_swap(
        &self,
        swap_id: &Hash256,
    ) -> Result<Option<LiquiditySwapRecord>, LiquidityStoreError> {
        let key = Self::liquidity_swap_key(swap_id);
        Ok(self
            .get(key)
            .map(|value| deserialize_from(value.as_ref(), "LiquiditySwapRecord")))
    }

    fn list_liquidity_swaps(
        &self,
        filter: LiquiditySwapFilter,
    ) -> Result<LiquiditySwapPage, LiquidityStoreError> {
        let prefix = if let Some(state) = filter.state {
            vec![LIQUIDITY_SWAP_STATE_PREFIX, liquidity_state_key(state)]
        } else {
            vec![LIQUIDITY_SWAP_PREFIX]
        };
        let rows = self.collect_by_prefix_with(&prefix, PrefixIterOptions::new());
        let swaps = rows
            .into_iter()
            .filter_map(|kv| {
                if prefix[0] == LIQUIDITY_SWAP_PREFIX {
                    Some(deserialize_from(kv.value.as_ref(), "LiquiditySwapRecord"))
                } else {
                    Self::parse_liquidity_swap_id_from_index(&kv.key)
                        .and_then(|swap_id| self.get_liquidity_swap(&swap_id).ok().flatten())
                }
            })
            .collect();

        Ok(LiquiditySwapPage {
            swaps,
            next_cursor: None,
        })
    }

    fn update_liquidity_swap_state(
        &self,
        swap_id: &Hash256,
        transition: LiquidityStateTransition,
    ) -> Result<(), LiquidityStoreError> {
        let mut swap = self
            .get_liquidity_swap(swap_id)?
            .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
        if !swap.state.can_transition_to(transition.state) {
            return Err(LiquidityStoreError::InvalidStateTransition {
                from: swap.state,
                to: transition.state,
            });
        }

        let old_state = swap.state;
        swap.state = transition.state;
        swap.updated_at = transition.updated_at;
        if transition.state == LiquiditySwapState::Failed {
            swap.failure_reason = transition.reason;
        }

        let mut batch = self.batch();
        batch.delete(Self::liquidity_swap_state_index_key(old_state, swap_id));
        let primary = KeyValue::LiquiditySwap(*swap_id, swap.clone());
        let state_index = KeyValue::LiquiditySwapStateIndex((swap.state, *swap_id));
        batch.put(primary.key(), primary.value());
        batch.put(state_index.key(), state_index.value());
        batch.commit();
        Ok(())
    }

    fn update_liquidity_swap(
        &self,
        swap_id: &Hash256,
        _update: LiquiditySwapUpdate,
    ) -> Result<(), LiquidityStoreError> {
        Err(LiquidityStoreError::SwapNotFound(*swap_id))
    }

    fn upsert_liquidity_asset(&self, _asset: LiquidityAsset) -> Result<(), LiquidityStoreError> {
        Err(LiquidityStoreError::Backend(
            "liquidity asset persistence unavailable in current task".to_string(),
        ))
    }

    fn get_liquidity_asset(
        &self,
        _asset_id: &str,
    ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
        Ok(None)
    }

    fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
        Ok(Vec::new())
    }
}

impl InvoiceStore for Store {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        let key = [&[CKB_INVOICE_PREFIX], id.as_ref()].concat();
        self.get(key).map(|v| deserialize_from(&v, "CkbInvoice"))
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let payment_hash = *invoice.payment_hash();
        if self.get_invoice(&payment_hash).is_some() {
            return Err(InvoiceError::DuplicatedInvoice(payment_hash.to_string()));
        }

        let mut batch = self.batch();
        let kv = KeyValue::CkbInvoice(payment_hash, invoice);
        batch.put(kv.key(), kv.value());
        let kv = KeyValue::CkbInvoiceStatus(payment_hash, CkbInvoiceStatus::Open);
        batch.put(kv.key(), kv.value());
        if let Some(preimage) = preimage {
            let kv = KeyValue::Preimage(payment_hash, preimage);
            batch.put(kv.key(), kv.value());
        }
        batch.commit();
        self.notify(StoreChange::PutCkbInvoiceStatus {
            payment_hash,
            invoice_status: CkbInvoiceStatus::Open,
        });
        if let Some(preimage) = preimage {
            self.notify(StoreChange::PutPreimage {
                payment_hash,
                payment_preimage: preimage,
            });
        }
        return Ok(());
    }

    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: crate::invoice::CkbInvoiceStatus,
    ) -> Result<(), InvoiceError> {
        self.get_invoice(id).ok_or(InvoiceError::InvoiceNotFound)?;
        let mut batch = self.batch();
        let kv = KeyValue::CkbInvoiceStatus(*id, status);
        batch.put(kv.key(), kv.value());
        batch.commit();
        self.notify(StoreChange::PutCkbInvoiceStatus {
            payment_hash: *id,
            invoice_status: status,
        });
        Ok(())
    }

    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus> {
        let key = [&[CKB_INVOICE_STATUS_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "CkbInvoiceStatus"))
    }
}

impl PreimageStore for Store {
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        let kv = KeyValue::Preimage(payment_hash, preimage);
        let key = kv.key();
        if let Some(existing) = self
            .get(&key)
            .map(|v| deserialize_from::<Hash256>(v.as_ref(), "Preimage"))
        {
            if existing == preimage {
                // Watchers are in-memory. Replaying the same persisted preimage after restart must
                // still emit PutPreimage so CCH/payment tracking can recover success events.
                self.notify(StoreChange::PutPreimage {
                    payment_hash,
                    payment_preimage: preimage,
                });
                return;
            }
            tracing::warn!(
                "Overwriting preimage for payment hash {:?}: existing value differs",
                payment_hash
            );
        }

        let mut batch = self.batch();
        batch.put(key, kv.value());
        batch.commit();
        self.notify(StoreChange::PutPreimage {
            payment_hash,
            payment_preimage: preimage,
        });
    }

    fn remove_preimage(&self, payment_hash: &Hash256) {
        let mut batch = self.batch();
        batch.delete([&[PREIMAGE_PREFIX], payment_hash.as_ref()].concat());
        batch.commit();
    }

    #[cfg(feature = "watchtower")]
    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        let key = [&[PREIMAGE_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Preimage"))
            .or_else(|| self.get_watch_preimage(&NodeId::local(), payment_hash))
    }

    #[cfg(not(feature = "watchtower"))]
    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        let key = [&[PREIMAGE_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Preimage"))
    }
}

impl NetworkGraphStateStore for Store {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        let prefix = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
        self.get(prefix)
            .map(|v| deserialize_from(v.as_ref(), "PaymentSession"))
            .map(|session: PaymentSession| session.init_attempts(self))
    }

    fn get_persisted_payment_status(&self, payment_hash: Hash256) -> Option<PaymentStatus> {
        let key = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from::<PaymentSession>(v.as_ref(), "PaymentSession").status)
    }

    fn get_all_payment_sessions(&self) -> Vec<PaymentSession> {
        let prefix = [PAYMENT_SESSION_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                let session: PaymentSession = deserialize_from(kv.value.as_ref(), "PaymentSession");
                session.init_attempts(self)
            })
            .collect()
    }

    fn get_payment_sessions_with_status(&self, status: PaymentStatus) -> Vec<PaymentSession> {
        let prefix = [PAYMENT_SESSION_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .filter_map(|kv| {
                let session: PaymentSession = deserialize_from(kv.value.as_ref(), "PaymentSession");
                if session.status == status {
                    Some(session.init_attempts(self))
                } else {
                    None
                }
            })
            .collect()
    }

    fn get_payment_sessions_with_limit(
        &self,
        limit: usize,
        after: Option<Hash256>,
        status: Option<PaymentStatus>,
    ) -> Vec<PaymentSession> {
        let prefix = [PAYMENT_SESSION_PREFIX];
        let start_key = after.map(|h| [&[PAYMENT_SESSION_PREFIX], h.as_ref()].concat());
        let iter = match start_key {
            Some(key) => self.prefix_iterator_from(prefix, key),
            None => self.prefix_iterator(prefix),
        };

        iter.filter_map(|(_key, value)| {
            let session: PaymentSession = deserialize_from(&value, "PaymentSession");
            match status {
                Some(ref s) if session.status != *s => None,
                _ => Some(session.init_attempts(self)),
            }
        })
        .take(limit)
        .collect()
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        let payment_hash = session.payment_hash();
        let session_clone = session.clone();
        let payment_preimage = (session.status == PaymentStatus::Success)
            .then(|| session.attempts().find_map(|attempt| attempt.preimage))
            .flatten();
        let mut batch = self.batch();
        let kv = KeyValue::PaymentSession(payment_hash, session);
        batch.put(kv.key(), kv.value());
        batch.commit();
        self.notify(StoreChange::PutPaymentSession {
            payment_hash,
            payment_session: session_clone,
            payment_preimage,
        });
    }

    fn get_attempt(&self, payment_hash: Hash256, attempt_id: u64) -> Option<Attempt> {
        let key = [
            &[ATTEMPT_PREFIX],
            payment_hash.as_ref(),
            &attempt_id.to_le_bytes(),
        ]
        .concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Attempt"))
    }

    fn insert_attempt(&self, attempt: Attempt) {
        assert_ne!(attempt.id, 0, "Attempt ID should not be zero");

        let first_hop_outpoint = attempt.first_hop_channel_outpoint().cloned();
        let is_new = self.get_attempt(attempt.payment_hash, attempt.id).is_none();

        let mut batch = self.batch();

        // Update the main attempt record
        let kv = KeyValue::Attempt((attempt.payment_hash, attempt.id), attempt.clone());
        batch.put(kv.key(), kv.value());

        // Add to channel index only for new attempts
        if is_new {
            if let Some(outpoint) = first_hop_outpoint {
                let kv =
                    KeyValue::AttemptChannelIndex((outpoint, attempt.payment_hash, attempt.id));
                batch.put(kv.key(), kv.value());
            }
        }

        batch.commit();
        self.notify(StoreChange::PutAttempt {
            payment_hash: attempt.payment_hash,
            attempt_status: attempt.status,
        });
    }

    fn get_attempts(&self, payment_hash: Hash256) -> Vec<Attempt> {
        let prefix = [&[ATTEMPT_PREFIX], payment_hash.as_ref()].concat();
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| deserialize_from(kv.value.as_ref(), "Attempt"))
            .collect()
    }

    fn delete_attempts(&self, payment_hash: Hash256) {
        let prefix = [&[ATTEMPT_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();

        // Get attempts to find their channel index entries
        let attempts: Vec<_> = self
            .collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                (
                    kv.key,
                    deserialize_from::<Attempt>(kv.value.as_ref(), "Attempt"),
                )
            })
            .collect();

        // Delete both main records and channel index entries
        for (key, attempt) in attempts {
            batch.delete(key);
            if let Some(outpoint) = attempt.first_hop_channel_outpoint() {
                let index_key = [
                    &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                    outpoint.as_slice(),
                    attempt.payment_hash.as_ref(),
                    &attempt.id.to_le_bytes(),
                ]
                .concat();
                batch.delete(index_key);
            }
        }

        batch.commit();
    }

    fn clear_attempts_channel_index(&self, payment_hash: Hash256) {
        let prefix = [&[ATTEMPT_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();

        // Get attempts to find their channel index entries
        let attempts: Vec<Attempt> = self
            .collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| deserialize_from(kv.value.as_ref(), "Attempt"))
            .collect();

        // Only delete channel index entries, keep the attempts themselves
        for attempt in attempts {
            if let Some(outpoint) = attempt.first_hop_channel_outpoint() {
                let index_key = [
                    &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                    outpoint.as_slice(),
                    attempt.payment_hash.as_ref(),
                    &attempt.id.to_le_bytes(),
                ]
                .concat();
                batch.delete(index_key);
            }
        }

        batch.commit();
    }

    fn get_pending_attempts_by_channel_outpoint(
        &self,
        channel_outpoint: &OutPoint,
    ) -> Vec<Attempt> {
        let prefix = [&[ATTEMPT_CHANNEL_INDEX_PREFIX], channel_outpoint.as_slice()].concat();

        self.collect_by_prefix(&prefix)
            .into_iter()
            .filter_map(|kv| {
                // Key format: [PREFIX, channel_outpoint(36 bytes), payment_hash(32 bytes), attempt_id(8 bytes)]
                // Extract payment_hash and attempt_id from key
                let key_slice: &[u8] = &kv.key;
                let outpoint_len = channel_outpoint.as_slice().len();
                let prefix_and_outpoint_len = 1 + outpoint_len;

                if key_slice.len() < prefix_and_outpoint_len + 32 + 8 {
                    return None;
                }

                let payment_hash_start = prefix_and_outpoint_len;
                let payment_hash_end = payment_hash_start + 32;
                let attempt_id_start = payment_hash_end;
                let attempt_id_end = attempt_id_start + 8;

                let payment_hash: Hash256 = (&key_slice[payment_hash_start..payment_hash_end])
                    .try_into()
                    .ok()?;
                let attempt_id = u64::from_le_bytes(
                    key_slice[attempt_id_start..attempt_id_end]
                        .try_into()
                        .ok()?,
                );

                let attempt = self.get_attempt(payment_hash, attempt_id)?;
                match attempt.status {
                    AttemptStatus::Retrying => Some(attempt),
                    AttemptStatus::Created
                        if !self.channel_owns_attempt(channel_outpoint, &attempt) =>
                    {
                        Some(attempt)
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: OutPoint,
        direction: Direction,
        result: TimedResult,
    ) {
        let mut batch = self.batch();
        let kv = KeyValue::PaymentHistoryTimedResult((channel_outpoint, direction), result);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn remove_channel_history(&mut self, channel_outpoint: &OutPoint) {
        let prefix = [
            &[PAYMENT_HISTORY_TIMED_RESULT_PREFIX],
            channel_outpoint.as_slice(),
        ]
        .concat();
        let mut batch = self.batch();
        for kv in self.collect_by_prefix(&prefix) {
            batch.delete(kv.key);
        }
        batch.commit();
    }

    fn get_payment_history_results(&self) -> Vec<(OutPoint, Direction, TimedResult)> {
        let prefix = vec![PAYMENT_HISTORY_TIMED_RESULT_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .map(|kv| {
                let channel_outpoint: OutPoint = OutPoint::from_slice(&kv.key[1..=36])
                    .expect("deserialize OutPoint should be OK");
                let direction = deserialize_from(&kv.key[37..], "Direction");
                let result = deserialize_from(kv.value.as_ref(), "TimedResult");
                (channel_outpoint, direction, result)
            })
            .collect()
    }
}

#[cfg(feature = "watchtower")]
impl WatchtowerStore for Store {
    fn get_watch_channels_with_nodes(&self) -> Vec<(NodeId, ChannelData)> {
        let prefix = vec![WATCHTOWER_CHANNEL_PREFIX];
        self.collect_by_prefix(&prefix)
            .into_iter()
            .filter_map(|kv| {
                let (node_id, _) = Self::parse_watchtower_channel_key(&kv.key)?;
                let channel_data = deserialize_from(kv.value.as_ref(), "ChannelData");
                Some((node_id, channel_data))
            })
            .collect()
    }

    fn insert_watch_channel(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        funding_udt_type_script: Option<Script>,
        local_settlement_key: Privkey,
        remote_settlement_key: Pubkey,
        local_funding_pubkey: Pubkey,
        remote_funding_pubkey: Pubkey,
        settlement_data: SettlementData,
    ) {
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let key = [
            &[WATCHTOWER_CHANNEL_PREFIX],
            node_id.as_ref(),
            channel_id.as_ref(),
        ]
        .concat();
        let value = serialize_to_vec(
            &ChannelData {
                channel_id,
                funding_udt_type_script,
                local_settlement_key,
                remote_settlement_key,
                local_funding_pubkey,
                remote_funding_pubkey,
                pending_remote_settlement_data: settlement_data.clone(),
                remote_settlement_data: settlement_data.clone(),
                local_settlement_data: settlement_data.clone(),
                revocation_data: None,
            },
            "ChannelData",
        );
        let mut batch = self.batch();
        batch.put(key, value);
        batch.commit();
    }

    fn remove_watch_channel(&self, node_id: NodeId, channel_id: Hash256) {
        // Only allow removing watchtower monitoring for closed channels.
        // Prevents accidental or malicious disabling of active channel protection.
        // Skipped in test builds to allow e2e tests to simulate stopping the watchtower.
        #[cfg(not(test))]
        if let Some(state) = self.get_channel_actor_state(&channel_id) {
            if !state.is_closed() {
                warn!(
                    "Refusing to remove watchtower for live channel {} (state: {:?})",
                    channel_id, state.state
                );
                return;
            }
        }
        let key = [
            &[WATCHTOWER_CHANNEL_PREFIX],
            node_id.as_ref(),
            channel_id.as_ref(),
        ]
        .concat();
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let payment_hashes = self
            .get(key.clone())
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
            .map(|channel_data| Self::watch_channel_payment_hashes(&channel_data))
            .unwrap_or_default();
        self.delete(key);
        self.cleanup_unused_watch_preimages_locked(
            &node_id,
            WatchtowerPreimageCleanupTarget::ExactSet(&payment_hashes),
            || {},
        );
    }

    fn update_revocation(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        revocation_data: RevocationData,
        remote_settlement_data: SettlementData,
    ) {
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let key = [
            &[WATCHTOWER_CHANNEL_PREFIX],
            node_id.as_ref(),
            channel_id.as_ref(),
        ]
        .concat();
        if let Some(mut channel_data) = self
            .get(key)
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
        {
            channel_data.remote_settlement_data = remote_settlement_data;
            channel_data.revocation_data = Some(revocation_data);
            let mut batch = self.batch();
            let kv = KeyValue::WatchtowerChannel(node_id, channel_id, channel_data);
            batch.put(kv.key(), kv.value());
            batch.commit();
        }
    }

    fn update_pending_remote_settlement(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        pending_remote_settlement_data: SettlementData,
    ) {
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let key = [
            &[WATCHTOWER_CHANNEL_PREFIX],
            node_id.as_ref(),
            channel_id.as_ref(),
        ]
        .concat();
        if let Some(mut channel_data) = self
            .get(key)
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
        {
            channel_data.pending_remote_settlement_data = pending_remote_settlement_data;
            let mut batch = self.batch();
            let kv = KeyValue::WatchtowerChannel(node_id, channel_id, channel_data);
            batch.put(kv.key(), kv.value());
            batch.commit();
        }
    }

    fn update_local_settlement(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        local_settlement_data: SettlementData,
    ) {
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let key = [
            &[WATCHTOWER_CHANNEL_PREFIX],
            node_id.as_ref(),
            channel_id.as_ref(),
        ]
        .concat();
        if let Some(mut channel_data) = self
            .get(key)
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
        {
            channel_data.local_settlement_data = local_settlement_data;
            let mut batch = self.batch();
            let kv = KeyValue::WatchtowerChannel(node_id, channel_id, channel_data);
            batch.put(kv.key(), kv.value());
            batch.commit();
        }
    }

    fn insert_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256, preimage: Hash256) {
        let lock = self.watchtower_write_lock(&node_id);
        let _guard = lock.lock();
        let mut batch = self.batch();
        let kv = KeyValue::WatchtowerPreimage(payment_hash, node_id.clone(), preimage);
        batch.put(kv.key(), kv.value());
        let kv = KeyValue::WatchtowerNodePaymentHash(node_id, payment_hash);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn remove_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256) {
        self.cleanup_unused_watch_preimages(
            Some(&node_id),
            WatchtowerPreimageCleanupTarget::Exact(&payment_hash),
        );
    }

    fn get_watch_preimage(&self, node_id: &NodeId, payment_hash: &Hash256) -> Option<Hash256> {
        self.get(Self::watchtower_preimage_key(node_id, payment_hash))
            .map(|v| deserialize_from(v.as_ref(), "Preimage"))
    }

    fn insert_onchain_tlc_settlement(
        &self,
        node_id: &NodeId,
        channel_id: &Hash256,
        tlc_id: TLCId,
        settlement: OnChainTlcSettlement,
    ) {
        let _guard = self.onchain_tlc_settlement_write_lock.lock();
        let key = Self::tlc_on_chain_settled_key(node_id, channel_id, tlc_id);
        if settlement.preimage.is_none() {
            if let Some(existing) = self.get(&key) {
                let existing: OnChainTlcSettlement =
                    deserialize_from(existing.as_ref(), "OnChainTlcSettlement");
                if existing.preimage.is_some() {
                    return;
                }
            }
        }

        let mut batch = self.batch();
        batch.put(key, serialize_to_vec(&settlement, "OnChainTlcSettlement"));
        batch.commit();
        drop(_guard);
        if settlement.preimage.is_none() {
            self.cleanup_unused_watch_preimages(
                None,
                WatchtowerPreimageCleanupTarget::Exact(&settlement.payment_hash),
            );
        }
    }

    fn get_onchain_tlc_settlement(
        &self,
        node_id: &NodeId,
        channel_id: &Hash256,
        tlc_id: TLCId,
        payment_hash: &Hash256,
    ) -> Option<StoredOnChainTlcSettlement> {
        if let Some(value) = self.get(Store::tlc_on_chain_settled_key(node_id, channel_id, tlc_id))
        {
            return Some(StoredOnChainTlcSettlement::Exact(deserialize_from(
                value.as_ref(),
                "OnChainTlcSettlement",
            )));
        }

        let payment_hash_prefix: [u8; 20] = payment_hash.as_ref()[0..20]
            .try_into()
            .expect("payment hash prefix");
        let value = self.get(Store::legacy_tlc_on_chain_settled_key(
            channel_id,
            &payment_hash_prefix,
        ))?;
        if value.is_empty() {
            return Some(StoredOnChainTlcSettlement::Legacy(
                LegacyOnChainTlcSettlement {
                    preimage: None,
                    tx_hash: None,
                    tlc_index: None,
                },
            ));
        }
        Some(StoredOnChainTlcSettlement::Legacy(deserialize_from(
            value.as_ref(),
            "LegacyOnChainTlcSettlement",
        )))
    }
}

impl GossipMessageStore for Store {
    fn get_broadcast_messages(
        &self,
        after_cursor: &Cursor,
        limit: usize,
    ) -> Vec<crate::fiber::types::BroadcastMessageWithTimestamp> {
        let cursor = after_cursor.to_bytes();
        let prefix = [BROADCAST_MESSAGE_PREFIX];
        let start = [&prefix, cursor.as_slice()].concat();
        let mut options = PrefixIterOptions::new()
            .start_key(&start)
            .start_key_exclusive();
        if limit > 0 {
            options = options.limit(limit);
        }
        self.collect_by_prefix_with(&prefix, options)
            .into_iter()
            .map(|kv| {
                debug_assert_eq!(kv.key.len(), 1 + CURSOR_SIZE);
                let mut timestamp_bytes = [0u8; 8];
                timestamp_bytes.copy_from_slice(&kv.key[1..9]);
                let timestamp = u64::from_be_bytes(timestamp_bytes);
                let message: BroadcastMessage =
                    deserialize_from(kv.value.as_ref(), "BroadcastMessage");
                (message, timestamp).into()
            })
            .collect::<Vec<_>>()
    }

    fn get_broadcast_messages_reverse(
        &self,
        before_cursor: Option<&Cursor>,
        limit: usize,
    ) -> Vec<crate::fiber::types::BroadcastMessageWithTimestamp> {
        let prefix = [BROADCAST_MESSAGE_PREFIX];
        let mut options = PrefixIterOptions::new().reverse().limit(limit);

        let start_cloned = before_cursor.map(|cursor| {
            let cursor_bytes = cursor.to_bytes();
            [&prefix, cursor_bytes.as_slice()].concat()
        });

        if let Some(start) = start_cloned.as_ref() {
            options = options.start_key(start).start_key_exclusive();
        }

        self.collect_by_prefix_with(&prefix, options)
            .into_iter()
            .map(|kv| {
                debug_assert_eq!(kv.key.len(), 1 + CURSOR_SIZE);
                let mut timestamp_bytes = [0u8; 8];
                timestamp_bytes.copy_from_slice(&kv.key[1..9]);
                let timestamp = u64::from_be_bytes(timestamp_bytes);
                let message: BroadcastMessage =
                    deserialize_from(kv.value.as_ref(), "BroadcastMessage");
                (message, timestamp).into()
            })
            .collect::<Vec<_>>()
    }

    fn get_broadcast_message_with_cursor(
        &self,
        cursor: &Cursor,
    ) -> Option<crate::fiber::types::BroadcastMessageWithTimestamp> {
        let key = [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat();
        self.get(key).map(|v| {
            let message: BroadcastMessage = deserialize_from(v.as_ref(), "BroadcastMessage");
            (message, cursor.timestamp).into()
        })
    }

    fn get_latest_broadcast_message_cursor(&self) -> Option<Cursor> {
        let prefix = vec![BROADCAST_MESSAGE_PREFIX];
        self.collect_by_prefix_with(&prefix, PrefixIterOptions::new().reverse().limit(1))
            .into_iter()
            .next()
            .map(|kv| {
                let last_key = kv.key.to_vec();
                Cursor::from_bytes(&last_key[1..]).expect("deserialize Cursor should be OK")
            })
    }

    fn get_latest_channel_announcement_timestamp(&self, outpoint: &OutPoint) -> Option<u64> {
        let key = get_channel_timestamps_key(outpoint);
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                key.as_slice(),
            ]
            .concat(),
        )
        .and_then(|v| {
            let v: [u8; 24] = v.try_into().expect("Invalid timestamp value length");
            let timestamp = u64::from_be_bytes(
                v[..8]
                    .try_into()
                    .expect("timestamp length valid, shown above"),
            );
            // The default timestamp value is 0.
            (timestamp != 0).then_some(timestamp)
        })
    }

    fn get_latest_channel_update_timestamp(
        &self,
        outpoint: &OutPoint,
        is_node1: bool,
    ) -> Option<u64> {
        let key = get_channel_timestamps_key(outpoint);
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                key.as_slice(),
            ]
            .concat(),
        )
        .map(|v| {
            let v: [u8; 24] = v.try_into().expect("Invalid timestamp value length");
            let start_index = if is_node1 { 8 } else { 16 };
            u64::from_be_bytes(
                v[start_index..start_index + 8]
                    .try_into()
                    .expect("timestamp length valid, shown above"),
            )
        })
    }

    fn get_latest_node_announcement_timestamp(
        &self,
        pk: &crate::fiber::types::Pubkey,
    ) -> Option<u64> {
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                BroadcastMessageID::NodeAnnouncement(*pk)
                    .to_bytes()
                    .as_slice(),
            ]
            .concat(),
        )
        .map(|v| u64::from_be_bytes(v.try_into().expect("Invalid timestamp value length")))
    }

    fn delete_broadcast_message(&self, cursor: &Cursor) {
        let key = [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat();
        let mut batch = self.batch();
        batch.delete(key);
        batch.commit();
    }

    fn save_channel_announcement(
        &self,
        timestamp: u64,
        channel_announcement: crate::fiber::types::ChannelAnnouncement,
    ) {
        if let Some(_old_timestamp) =
            self.get_latest_channel_announcement_timestamp(&channel_announcement.channel_outpoint)
        {
            // Channel announcement is immutable. If we have already saved one channel announcement,
            // we can early return now.
            return;
        }

        let mut batch = self.batch();

        update_channel_timestamp(
            self,
            &mut batch,
            &channel_announcement.channel_outpoint,
            timestamp,
            ChannelTimestamp::ChannelAnnouncement(),
        );

        let kv = KeyValue::BroadcastMessage(
            Cursor::new(
                timestamp,
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                ),
            ),
            BroadcastMessage::ChannelAnnouncement(channel_announcement),
        );
        batch.put(kv.key(), kv.value());

        batch.commit();
    }

    fn save_channel_update(&self, channel_update: crate::fiber::types::ChannelUpdate) {
        let mut batch = self.batch();
        let message_id = BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone());

        // Remove old channel update if exists
        if let Some(old_timestamp) = self.get_latest_channel_update_timestamp(
            &channel_update.channel_outpoint,
            channel_update.is_update_of_node_1(),
        ) {
            if channel_update.timestamp <= old_timestamp {
                // This is an outdated channel update, early return
                return;
            }
            // Delete old channel update
            batch.delete(
                [
                    &[BROADCAST_MESSAGE_PREFIX],
                    Cursor::new(old_timestamp, message_id.clone())
                        .to_bytes()
                        .as_slice(),
                ]
                .concat(),
            );
        }

        update_channel_timestamp(
            self,
            &mut batch,
            &channel_update.channel_outpoint,
            channel_update.timestamp,
            if channel_update.is_update_of_node_1() {
                ChannelTimestamp::ChannelUpdateOfNode1()
            } else {
                ChannelTimestamp::ChannelUpdateOfNode2()
            },
        );

        // Save the channel update
        let kv = KeyValue::BroadcastMessage(
            Cursor::new(channel_update.timestamp, message_id),
            BroadcastMessage::ChannelUpdate(channel_update),
        );
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn save_node_announcement(&self, node_announcement: crate::fiber::types::NodeAnnouncement) {
        debug_assert!(
            node_announcement.verify(),
            "Node announcement must be verified: {:?}",
            node_announcement
        );
        let mut batch = self.batch();
        let message_id = BroadcastMessageID::NodeAnnouncement(node_announcement.node_id);

        if let Some(old_timestamp) =
            self.get_latest_node_announcement_timestamp(&node_announcement.node_id)
        {
            if node_announcement.timestamp <= old_timestamp {
                // This is an outdated node announcement. Early return.
                return;
            }

            // Delete old node announcement
            batch.delete(
                [
                    &[BROADCAST_MESSAGE_PREFIX],
                    Cursor::new(old_timestamp, message_id.clone())
                        .to_bytes()
                        .as_slice(),
                ]
                .concat(),
            );
        }
        let kv = KeyValue::BroadcastMessageTimestamp(
            BroadcastMessageID::NodeAnnouncement(node_announcement.node_id),
            node_announcement.timestamp,
        );
        batch.put(kv.key(), kv.value());

        let kv = KeyValue::BroadcastMessage(
            Cursor::new(node_announcement.timestamp, message_id.clone()),
            BroadcastMessage::NodeAnnouncement(node_announcement.clone()),
        );
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn get_channel_timestamps_iter(&self) -> impl IntoIterator<Item = (OutPoint, [u64; 3])> {
        // 0 is used to get timestamps for channels instead of node announcements.
        const PREFIX: [u8; 2] = [BROADCAST_MESSAGE_TIMESTAMP_PREFIX, 0];
        self.collect_by_prefix(&PREFIX).into_iter().map(|kv| {
            let outpoint =
                OutPoint::from_slice(&kv.key[2..]).expect("deserialize OutPoint should be OK");
            assert_eq!(kv.value.len(), 24);
            let timestamps = [
                u64::from_be_bytes(kv.value[0..8].try_into().unwrap()),
                u64::from_be_bytes(kv.value[8..16].try_into().unwrap()),
                u64::from_be_bytes(kv.value[16..24].try_into().unwrap()),
            ];
            (outpoint, timestamps)
        })
    }

    fn delete_channel_timestamps(&self, outpoint: &OutPoint) {
        let key = get_channel_timestamps_key(outpoint);
        let mut batch = self.batch();
        batch.delete([&[BROADCAST_MESSAGE_TIMESTAMP_PREFIX], key.as_slice()].concat());
        batch.commit();
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl CchOrderStore for Store {
    fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchStoreError> {
        let key = [&[CCH_ORDER_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(&v, "CchOrder"))
            .ok_or(CchStoreError::NotFound(*payment_hash))
    }

    fn insert_cch_order(&self, order: CchOrder) -> Result<(), CchStoreError> {
        let key = [&[CCH_ORDER_PREFIX], order.payment_hash.as_ref()].concat();
        if self.get(&key).is_some() {
            return Err(CchStoreError::Duplicated(order.payment_hash));
        }
        let mut batch = self.batch();
        let kv = KeyValue::CchOrder(order.payment_hash, order);
        batch.put(kv.key(), kv.value());
        batch.commit();
        Ok(())
    }

    fn update_cch_order(&self, order: CchOrder) {
        let mut batch = self.batch();
        let kv = KeyValue::CchOrder(order.payment_hash, order);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    fn get_cch_order_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        const PREFIX_LEN: usize = 1;
        const PREFIX: [u8; PREFIX_LEN] = [CCH_ORDER_PREFIX];
        self.collect_by_prefix(&PREFIX).into_iter().map(|kv| {
            Hash256::try_from(&kv.key[PREFIX_LEN..]).expect("CchOrder key must be Hash256")
        })
    }

    fn delete_cch_order(&self, payment_hash: &Hash256) {
        let key = [&[CCH_ORDER_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();
        batch.delete(key);
        batch.commit();
    }

    fn get_receive_btc_order_creation(
        &self,
        payment_hash: &Hash256,
    ) -> Result<CchReceiveBtcOrderCreation, CchStoreError> {
        let key = [
            &[CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX],
            payment_hash.as_ref(),
        ]
        .concat();
        self.get(key)
            .map(|value| deserialize_from(&value, "CchReceiveBtcOrderCreation"))
            .ok_or(CchStoreError::NotFound(*payment_hash))
    }

    fn insert_receive_btc_order_creation(
        &self,
        creation: CchReceiveBtcOrderCreation,
    ) -> Result<(), CchStoreError> {
        let key = [
            &[CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX],
            creation.payment_hash.as_ref(),
        ]
        .concat();
        if self.get(&key).is_some() {
            return Err(CchStoreError::Duplicated(creation.payment_hash));
        }
        let mut batch = self.batch();
        let kv = KeyValue::CchReceiveBtcOrderCreation(creation.payment_hash, creation);
        batch.put(kv.key(), kv.value());
        batch.commit();
        Ok(())
    }

    fn get_receive_btc_order_creation_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        const PREFIX_LEN: usize = 1;
        const PREFIX: [u8; PREFIX_LEN] = [CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX];
        self.collect_by_prefix(&PREFIX).into_iter().map(|kv| {
            Hash256::try_from(&kv.key[PREFIX_LEN..])
                .expect("CchReceiveBtcOrderCreation key must be Hash256")
        })
    }

    fn complete_receive_btc_order_creation(&self, order: CchOrder) -> Result<(), CchStoreError> {
        let order_key = [&[CCH_ORDER_PREFIX], order.payment_hash.as_ref()].concat();
        if self.get(&order_key).is_some() {
            return Err(CchStoreError::Duplicated(order.payment_hash));
        }

        let creation_key = [
            &[CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX],
            order.payment_hash.as_ref(),
        ]
        .concat();
        let mut batch = self.batch();
        let kv = KeyValue::CchOrder(order.payment_hash, order);
        batch.put(kv.key(), kv.value());
        batch.delete(creation_key);
        batch.commit();
        Ok(())
    }

    fn delete_receive_btc_order_creation(&self, payment_hash: &Hash256) {
        let key = [
            &[CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX],
            payment_hash.as_ref(),
        ]
        .concat();
        let mut batch = self.batch();
        batch.delete(key);
        batch.commit();
    }

    fn get_send_btc_order_creation(
        &self,
        payment_hash: &Hash256,
    ) -> Result<CchSendBtcOrderCreation, CchStoreError> {
        let key = [&[CCH_SEND_BTC_ORDER_CREATION_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|value| deserialize_from(&value, "CchSendBtcOrderCreation"))
            .ok_or(CchStoreError::NotFound(*payment_hash))
    }

    fn insert_send_btc_order_creation(
        &self,
        creation: CchSendBtcOrderCreation,
    ) -> Result<(), CchStoreError> {
        let key = [
            &[CCH_SEND_BTC_ORDER_CREATION_PREFIX],
            creation.payment_hash.as_ref(),
        ]
        .concat();
        if self.get(&key).is_some() {
            return Err(CchStoreError::Duplicated(creation.payment_hash));
        }
        let mut batch = self.batch();
        let kv = KeyValue::CchSendBtcOrderCreation(creation.payment_hash, creation);
        batch.put(kv.key(), kv.value());
        batch.commit();
        Ok(())
    }

    fn get_send_btc_order_creation_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        const PREFIX_LEN: usize = 1;
        const PREFIX: [u8; PREFIX_LEN] = [CCH_SEND_BTC_ORDER_CREATION_PREFIX];
        self.collect_by_prefix(&PREFIX).into_iter().map(|kv| {
            Hash256::try_from(&kv.key[PREFIX_LEN..])
                .expect("CchSendBtcOrderCreation key must be Hash256")
        })
    }

    fn complete_send_btc_order_creation(&self, order: CchOrder) -> Result<(), CchStoreError> {
        let order_key = [&[CCH_ORDER_PREFIX], order.payment_hash.as_ref()].concat();
        if self.get(&order_key).is_some() {
            return Err(CchStoreError::Duplicated(order.payment_hash));
        }

        let creation_key = [
            &[CCH_SEND_BTC_ORDER_CREATION_PREFIX],
            order.payment_hash.as_ref(),
        ]
        .concat();
        let mut batch = self.batch();
        let kv = KeyValue::CchOrder(order.payment_hash, order);
        batch.put(kv.key(), kv.value());
        batch.delete(creation_key);
        batch.commit();
        Ok(())
    }

    fn delete_send_btc_order_creation(&self, payment_hash: &Hash256) {
        let key = [&[CCH_SEND_BTC_ORDER_CREATION_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();
        batch.delete(key);
        batch.commit();
    }
}

// All timestamps are saved in a 24-byte array, with BroadcastMessageID::ChannelAnnouncement(outpoint) as the key.
// the first 8 bytes in the 24 bytes is the timestamp for channel announcement, the second 8 bytes
// is the timestamp for channel update of node 1 and the last 8 bytes for channel update of node 2.
// TODO: previous implementation accidentally used BroadcastMessageID::ChannelUpdate as the key
// for the channel updates timestamps. I have fixed it here by using the same key as the channel
// announcement. This is a breaking change, we need migration for this.
pub(crate) fn get_channel_timestamps_key(outpoint: &OutPoint) -> Vec<u8> {
    BroadcastMessageID::ChannelAnnouncement(outpoint.clone())
        .to_bytes()
        .to_vec()
}

fn update_channel_timestamp(
    store: &Store,
    batch: &mut <Store as StorageBackend>::Batch,
    outpoint: &OutPoint,
    timestamp: u64,
    channel_timestamp: ChannelTimestamp,
) {
    let offset = match channel_timestamp {
        ChannelTimestamp::ChannelAnnouncement() => 0,
        ChannelTimestamp::ChannelUpdateOfNode1() => 8,
        ChannelTimestamp::ChannelUpdateOfNode2() => 16,
    };
    let message_id = get_channel_timestamps_key(outpoint);

    let timestamp_key = [&[BROADCAST_MESSAGE_TIMESTAMP_PREFIX], message_id.as_slice()].concat();
    let mut timestamps = store
        .get(&timestamp_key)
        .map(|v: Vec<u8>| v.try_into().expect("Invalid timestamp value length"))
        .unwrap_or([0u8; 24]);
    timestamps[offset..offset + 8].copy_from_slice(&timestamp.to_be_bytes());
    batch.put(timestamp_key, timestamps);
}

#[cfg(all(test, feature = "watchtower", not(target_arch = "wasm32")))]
mod watchtower_preimage_gc_tests {
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    use fiber_types::{Hash256, NodeId};
    use tempfile::tempdir;

    use crate::watchtower::WatchtowerStore;

    use super::{open_store, WatchtowerPreimageCleanupTarget};

    #[test]
    fn concurrent_preimage_insert_is_not_deleted_by_stale_gc_decision() {
        let path = tempdir().expect("temp directory");
        let store = open_store(path.path()).expect("open store");
        let node_id = NodeId::local();
        let payment_hash = Hash256::from([1; 32]);
        let old_preimage = Hash256::from([2; 32]);
        let new_preimage = Hash256::from([3; 32]);
        store.insert_watch_preimage(node_id.clone(), payment_hash, old_preimage);

        let concurrent_store = store.clone();
        let concurrent_node_id = node_id.clone();
        let before_insert = Arc::new(Barrier::new(2));
        let concurrent_before_insert = Arc::clone(&before_insert);
        let (finished_tx, finished_rx) = mpsc::channel();
        let insert = std::thread::spawn(move || {
            concurrent_before_insert.wait();
            concurrent_store.insert_watch_preimage(concurrent_node_id, payment_hash, new_preimage);
            finished_tx.send(()).expect("signal insert finish");
        });

        store.cleanup_unused_watch_preimages_with_hook(
            &node_id,
            WatchtowerPreimageCleanupTarget::Exact(&payment_hash),
            || {
                before_insert.wait();
                assert!(
                    finished_rx
                        .recv_timeout(Duration::from_millis(100))
                        .is_err(),
                    "concurrent insert must wait for the GC delete commit"
                );
            },
        );
        insert.join().expect("concurrent insert thread");

        assert_eq!(
            store.get_watch_preimage(&node_id, &payment_hash),
            Some(new_preimage),
            "GC must not delete a preimage committed after its liveness decision"
        );
    }

    #[test]
    fn one_node_preimage_gc_does_not_block_another_node_writer() {
        let path = tempdir().expect("temp directory");
        let store = open_store(path.path()).expect("open store");
        let cleanup_node_id = NodeId::from_bytes(vec![1]);
        let writer_node_id = NodeId::from_bytes(vec![2]);
        let cleanup_payment_hash = Hash256::from([4; 32]);
        let writer_payment_hash = Hash256::from([5; 32]);
        let writer_preimage = Hash256::from([6; 32]);
        store.insert_watch_preimage(
            cleanup_node_id.clone(),
            cleanup_payment_hash,
            Hash256::from([7; 32]),
        );

        let concurrent_store = store.clone();
        let concurrent_writer_node_id = writer_node_id.clone();
        let before_insert = Arc::new(Barrier::new(2));
        let concurrent_before_insert = Arc::clone(&before_insert);
        let (finished_tx, finished_rx) = mpsc::channel();
        let insert = std::thread::spawn(move || {
            concurrent_before_insert.wait();
            concurrent_store.insert_watch_preimage(
                concurrent_writer_node_id,
                writer_payment_hash,
                writer_preimage,
            );
            finished_tx.send(()).expect("signal insert finish");
        });

        store.cleanup_unused_watch_preimages_with_hook(
            &cleanup_node_id,
            WatchtowerPreimageCleanupTarget::Exact(&cleanup_payment_hash),
            || {
                before_insert.wait();
                finished_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("another node's writer must not wait for this node's GC");
            },
        );
        insert.join().expect("concurrent insert thread");

        assert_eq!(
            store.get_watch_preimage(&writer_node_id, &writer_payment_hash),
            Some(writer_preimage),
            "another node's preimage write must complete independently"
        );
    }
}
