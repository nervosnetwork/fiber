use fiber_v070::{
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use crate::util::convert;

use fiber_v060::fiber::channel::{
    ChannelActorState as OldChannelActorState, ForwardTlc as OldForwardTlc, TLCId as OldTLCId,
    TlcInfo as OldTlcInfo, TlcState as OldTlcState,
};
use fiber_v061::fiber::channel::{
    ChannelActorState as NewChannelActorState, PendingTlcs as NewPendingTlcs, TLCId as NewTLCId,
    TlcInfo as NewTlcInfo, TlcState as NewTlcState,
};

use fiber_v060::fiber::types::{Hash256, PeeledPaymentOnionPacket};

// Remember to update the version number here, sample `20311116135521`
const MIGRATION_DB_VERSION: &str = "20251219152333";

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

        info!("migrate ChannelActorState ...");
        const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];
        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            if bincode::deserialize::<NewChannelActorState>(&v).is_ok() {
                continue;
            }

            let old: OldChannelActorState =
                bincode::deserialize(&v).expect("deserialize to old channel state");
            let new = migrate_channel_state(old);

            let new_bytes = bincode::serialize(&new).expect("serialize to new channel state");
            db.put(k, new_bytes);
        }

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}

fn migrate_channel_state(old: OldChannelActorState) -> NewChannelActorState {
    NewChannelActorState {
        state: convert(old.state),
        local_pubkey: convert(old.local_pubkey),
        remote_pubkey: convert(old.remote_pubkey),
        id: convert(old.id),
        funding_tx: old.funding_tx,
        funding_tx_confirmed_at: old.funding_tx_confirmed_at,
        funding_udt_type_script: old.funding_udt_type_script,
        is_acceptor: old.is_acceptor,
        to_local_amount: old.to_local_amount,
        to_remote_amount: old.to_remote_amount,
        local_reserved_ckb_amount: old.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: old.remote_reserved_ckb_amount,
        commitment_fee_rate: old.commitment_fee_rate,
        commitment_delay_epoch: old.commitment_delay_epoch,
        funding_fee_rate: old.funding_fee_rate,
        signer: convert(old.signer),
        local_channel_public_keys: convert(old.local_channel_public_keys),
        commitment_numbers: convert(old.commitment_numbers),
        local_constraints: convert(old.local_constraints),
        remote_constraints: convert(old.remote_constraints),
        tlc_state: migrate_tlc_state(old.tlc_state),
        remote_shutdown_script: old.remote_shutdown_script,
        local_shutdown_script: old.local_shutdown_script,
        last_committed_remote_nonce: old.last_committed_remote_nonce,
        remote_revocation_nonce_for_next: old.remote_revocation_nonce_for_next,
        remote_revocation_nonce_for_send: old.remote_revocation_nonce_for_send,
        remote_revocation_nonce_for_verify: old.remote_revocation_nonce_for_verify,
        retryable_tlc_operations: convert(old.retryable_tlc_operations),
        shutdown_transaction_hash: convert(old.shutdown_transaction_hash),
        waiting_forward_tlc_tasks: migrate_waiting_forward_tlc_tasks(old.waiting_forward_tlc_tasks),
        latest_commitment_transaction: old.latest_commitment_transaction,
        remote_commitment_points: convert(old.remote_commitment_points),
        remote_channel_public_keys: convert(old.remote_channel_public_keys),
        local_shutdown_info: convert(old.local_shutdown_info),
        remote_shutdown_info: convert(old.remote_shutdown_info),
        reestablishing: old.reestablishing,
        last_revoke_ack_msg: convert(old.last_revoke_ack_msg),
        created_at: old.created_at,
        public_channel_info: convert(old.public_channel_info),
        local_tlc_info: convert(old.local_tlc_info),
        remote_tlc_info: convert(old.remote_tlc_info),
        waiting_peer_response: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        ephemeral_config: Default::default(),
        private_key: None,
    }
}

fn migrate_tlc_state(old: OldTlcState) -> NewTlcState {
    NewTlcState {
        offered_tlcs: NewPendingTlcs {
            tlcs: old
                .offered_tlcs
                .tlcs
                .into_iter()
                .map(migrate_tlc_info)
                .collect(),
            next_tlc_id: old.offered_tlcs.next_tlc_id,
        },
        received_tlcs: NewPendingTlcs {
            tlcs: old
                .received_tlcs
                .tlcs
                .into_iter()
                .map(migrate_tlc_info)
                .collect(),
            next_tlc_id: old.received_tlcs.next_tlc_id,
        },
        waiting_ack: old.waiting_ack,
    }
}

fn migrate_tlc_info(old: OldTlcInfo) -> NewTlcInfo {
    NewTlcInfo {
        status: convert(old.status),
        tlc_id: convert(old.tlc_id),
        amount: old.amount,
        payment_hash: convert(old.payment_hash),
        total_amount: old.total_amount,
        payment_secret: convert(old.payment_secret),
        attempt_id: old.attempt_id,
        expiry: old.expiry,
        hash_algorithm: convert(old.hash_algorithm),
        onion_packet: convert(old.onion_packet),
        shared_secret: old.shared_secret,
        created_at: convert(old.created_at),
        removed_reason: convert(old.removed_reason),
        forwarding_tlc: old.previous_tlc.map(|(channel_id, tlc_id)| {
            let id = match tlc_id {
                OldTLCId::Offered(id) => id,
                OldTLCId::Received(id) => id,
            };
            (convert(channel_id), id)
        }),
        removed_confirmed_at: old.removed_confirmed_at,
        applied_flags: convert(old.applied_flags),
    }
}

fn migrate_waiting_forward_tlc_tasks(
    old: HashMap<(Hash256, OldTLCId), OldForwardTlc>,
) -> HashMap<NewTLCId, [u8; 32]> {
    old.into_iter()
        .map(|((_channel_id, tlc_id), old_forward_tlc)| {
            let ForwardTlc(_, _, peeled, _) = convert(old_forward_tlc);
            (convert(tlc_id), peeled.shared_secret)
        })
        .collect()
}

// v0.6.0 struct fields are private, have to create a new struct for migration
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ForwardTlc(Hash256, OldTLCId, PeeledPaymentOnionPacket, u128);
