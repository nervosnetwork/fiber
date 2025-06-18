use fiber::store::Store;
use fiber::{store::migration::Migration, Error};
use fiber_v061::fiber::types::Pubkey;
use fiber_v061::store::store_impl::StoreKeyValue;
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250617093908";

pub use fiber_v051::fiber::channel::ChannelActorState as OldChannelActorState;
pub use fiber_v051::fiber::channel::PendingTlcs as OldPendingTlcs;
pub use fiber_v051::fiber::channel::TlcInfo as OldTlcInfo;
pub use fiber_v051::fiber::channel::TlcState as OldTlcState;
pub use fiber_v051::fiber::graph::PaymentSession as OldPaymentSession;
pub use fiber_v051::fiber::graph::PaymentSessionStatus as OldPaymentSessionStatus;
pub use fiber_v051::fiber::graph::SessionRoute as OldSessionRoute;
pub use fiber_v051::fiber::graph::SessionRouteNode as OldSessionRouteNode;
pub use fiber_v051::fiber::network::HopHint as OldHopHint;
pub use fiber_v051::fiber::network::SendPaymentData as OldPaymentData;
pub use fiber_v051::fiber::types::Hash256 as OldHash256;

pub use fiber_v061::fiber::channel::ChannelActorState as NewChannelActorState;
pub use fiber_v061::fiber::channel::PendingTlcs as NewPendingTlcs;
pub use fiber_v061::fiber::channel::TlcInfo as NewTlcInfo;
pub use fiber_v061::fiber::channel::TlcState as NewTlcState;
pub use fiber_v061::fiber::graph::Attempt;
pub use fiber_v061::fiber::graph::PaymentSession as NewPaymentSession;
pub use fiber_v061::fiber::graph::PaymentSessionStatus as NewPaymentSessionStatus;
pub use fiber_v061::fiber::graph::SessionRoute as NewSessionRoute;
pub use fiber_v061::fiber::graph::SessionRouteNode as NewSessionRouteNode;
pub use fiber_v061::fiber::network::HopHint as NewHopHint;
pub use fiber_v061::fiber::network::SendPaymentData as NewPaymentData;
pub use fiber_v061::fiber::types::Hash256 as NewHash256;
pub use fiber_v061::store::store_impl::KeyValue as NewKeyValue;

pub struct MigrationObj {
    version: String,
}

impl Default for MigrationObj {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationObj {
    pub fn new() -> Self {
        Self {
            version: MIGRATION_DB_VERSION.to_string(),
        }
    }
}

impl Migration for MigrationObj {
    fn migrate<'a>(
        &self,
        db: &'a Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a Store, Error> {
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        // NodeAnnouncement is changed, we need to delete the old broadcast messages
        info!("clear broadcast messages ...");
        delete_broadcast_messages(db);

        info!("migrate payment session ...");
        migrate_payment_session(db);

        info!("migrate channel actor state ...");
        migrate_channel_actor_state(db);

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}

fn delete_broadcast_messages(db: &Store) {
    const BROADCAST_MESSAGE_PREFIX: u8 = 96;
    let prefix = vec![BROADCAST_MESSAGE_PREFIX];

    for (k, _v) in db
        .prefix_iterator(prefix.clone().as_slice())
        .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
    {
        // just delete the old broadcast message
        db.delete(k);
    }
}

fn convert_send_payment_data(request: OldPaymentData) -> NewPaymentData {
    let mut value = serde_json::to_value(&request).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.insert("allow_mpp".to_string(), serde_json::json!(true));
    serde_json::from_value(value).unwrap()
}

fn convert_payment_session_status(status: OldPaymentSessionStatus) -> NewPaymentSessionStatus {
    match status {
        OldPaymentSessionStatus::Created => NewPaymentSessionStatus::Created,
        OldPaymentSessionStatus::Inflight => NewPaymentSessionStatus::Inflight,
        OldPaymentSessionStatus::Success => NewPaymentSessionStatus::Success,
        OldPaymentSessionStatus::Failed => NewPaymentSessionStatus::Failed,
    }
}

fn convert_session_route(route: OldSessionRoute) -> NewSessionRoute {
    let OldSessionRoute { nodes } = route;
    let nodes = nodes
        .into_iter()
        .map(|node| {
            let OldSessionRouteNode {
                pubkey,
                amount,
                channel_outpoint,
            } = node;
            NewSessionRouteNode {
                pubkey: Pubkey::from(pubkey.0),
                amount,
                channel_outpoint,
            }
        })
        .collect();
    NewSessionRoute { nodes }
}

fn convert_payment_session(session: OldPaymentSession) -> NewPaymentSession {
    let OldPaymentSession {
        request,
        retried_times,
        last_error,
        try_limit,
        status,
        created_at,
        last_updated_at,
        route,
        session_key,
    } = session;

    let request = convert_send_payment_data(request);
    let route = convert_session_route(route);
    let status = convert_payment_session_status(status);

    let attempt = Attempt {
        id: 1,
        try_limit,
        tried_times: retried_times,
        hash: request.payment_hash,
        status,
        payment_hash: request.payment_hash,
        route,
        session_key,
        preimage: request.preimage,
        created_at,
        last_updated_at,
        last_error: last_error.clone(),
    };

    NewPaymentSession {
        request,
        last_error,
        try_limit,
        status,
        created_at,
        last_updated_at,
        cached_attempts: vec![attempt],
    }
}

fn migrate_payment_session(db: &Store) {
    const PAYMENT_SESSION_PREFIX: u8 = 192;
    let prefix = [PAYMENT_SESSION_PREFIX];

    for (key, value) in db.prefix_iterator(&prefix) {
        let session: OldPaymentSession =
            bincode::deserialize_from(value.as_ref()).expect("PaymentSession");
        let new = convert_payment_session(session);
        let new_bytes = bincode::serialize(&new).expect("serialize");

        // save the new payment session
        db.put(key, new_bytes);

        // save the new attempts
        for attempt in new.attempts() {
            let kv = NewKeyValue::Attempt((attempt.payment_hash, attempt.id), attempt.clone());
            db.put(kv.key(), kv.value());
        }
    }
}

fn convert_pending_tlcs(pending_tlcs: OldPendingTlcs) -> NewPendingTlcs {
    let OldPendingTlcs { tlcs, next_tlc_id } = pending_tlcs;
    let tlcs = tlcs
        .into_iter()
        .map(|tlc| {
            let mut value = serde_json::to_value(&tlc).unwrap();
            let obj = value.as_object_mut().unwrap();

            obj.insert("attempt_id".to_string(), serde_json::Value::Null);
            obj.insert("total_amount".to_string(), serde_json::json!("null"));
            obj.insert("payment_secret".to_string(), serde_json::json!("null"));

            serde_json::from_value(value).unwrap()
        })
        .collect();
    NewPendingTlcs { tlcs, next_tlc_id }
}

fn convert_tlc_state(tlc_state: OldTlcState) -> NewTlcState {
    let mut value = serde_json::to_value(&tlc_state).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.insert(
        "offered_tlcs".to_string(),
        serde_json::to_value(convert_pending_tlcs(tlc_state.offered_tlcs)).unwrap(),
    );
    obj.insert(
        "received_tlcs".to_string(),
        serde_json::to_value(convert_pending_tlcs(tlc_state.received_tlcs)).unwrap(),
    );

    serde_json::from_value(value).unwrap()
}

fn convert_channel_actor_state(state: OldChannelActorState) -> NewChannelActorState {
    let mut value = serde_json::to_value(&state).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.insert(
        "tlc_state".to_string(),
        serde_json::to_value(convert_tlc_state(state.tlc_state)).unwrap(),
    );

    serde_json::from_value(value).unwrap()
}

fn migrate_channel_actor_state(db: &Store) {
    const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
    let prefix = [CHANNEL_ACTOR_STATE_PREFIX];

    for (key, value) in db.prefix_iterator(&prefix) {
        let state: OldChannelActorState =
            bincode::deserialize_from(value.as_ref()).expect("ChannelActorState");
        let new = convert_channel_actor_state(state);
        let new_bytes = bincode::serialize(&new).expect("serialize");

        db.put(key, new_bytes);
    }
}
