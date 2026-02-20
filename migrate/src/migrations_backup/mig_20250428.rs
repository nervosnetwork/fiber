use fiber::store::Store;
use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250428012453";

use crate::util::convert;

pub use fiber_v050::fiber::channel::ChannelActorState as OldChannelActorState;
pub use fiber_v051::fiber::channel::ChannelActorState as NewChannelActorState;

pub use fiber_v020::fiber::graph::PaymentSession as OldPaymentSessionV020;
pub use fiber_v020::fiber::network::SendPaymentData as OldSendPaymentDataV020;
pub use fiber_v050::fiber::graph::PaymentSession as OldPaymentSessionV050;
pub use fiber_v050::fiber::network::SendPaymentData as OldSendPaymentDataV050;
pub use fiber_v051::fiber::graph::PaymentSession as NewPaymentSession;
pub use fiber_v051::fiber::network::SendPaymentData as NewSendPaymentData;

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

        info!("migrate PaymentSession ...");
        const PAYMENT_SESSION_PREFIX: u8 = 192;
        let prefix = vec![PAYMENT_SESSION_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            if bincode::deserialize::<NewPaymentSession>(&v).is_ok() {
                // if we can deserialize the data correctly with new version, just skip it.
                continue;
            }

            // Try to deserialize with older versions and migrate if successful
            let new_payment_session =
                if let Ok(session) = bincode::deserialize::<OldPaymentSessionV020>(&v) {
                    migrate_payment_session_v020(session)
                } else if let Ok(session) = bincode::deserialize::<OldPaymentSessionV050>(&v) {
                    migrate_payment_session_v050(session)
                } else {
                    panic!("failed to deserialize payment session");
                };

            // Save the migrated payment session
            let new_payment_session_bytes =
                bincode::serialize(&new_payment_session).expect("serialize to new payment session");
            db.put(k, new_payment_session_bytes);
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
        tlc_state: convert(old.tlc_state),
        remote_shutdown_script: old.remote_shutdown_script,
        local_shutdown_script: old.local_shutdown_script,
        last_commitment_signed_remote_nonce: old.last_commitment_signed_remote_nonce,
        last_revoke_and_ack_remote_nonce: old.last_revoke_and_ack_remote_nonce,
        last_committed_remote_nonce: old.last_committed_remote_nonce,
        latest_commitment_transaction: old.latest_commitment_transaction,
        remote_commitment_points: convert(old.remote_commitment_points),
        remote_channel_public_keys: convert(old.remote_channel_public_keys),
        local_shutdown_info: convert(old.local_shutdown_info),
        remote_shutdown_info: convert(old.remote_shutdown_info),
        reestablishing: old.reestablishing,
        last_revoke_ack_msg: None,
        created_at: old.created_at,
        public_channel_info: convert(old.public_channel_info),
        local_tlc_info: convert(old.local_tlc_info),
        remote_tlc_info: convert(old.remote_tlc_info),
        network: None,
        scheduled_channel_update_handle: None,
        waiting_peer_response: None,
    }
}

fn migrate_payment_session_v020(old: OldPaymentSessionV020) -> NewPaymentSession {
    let old_request = old.request.clone();

    let request = NewSendPaymentData {
        target_pubkey: convert(old_request.target_pubkey),
        amount: old_request.amount,
        payment_hash: convert(old_request.payment_hash),
        invoice: old_request.invoice,
        final_tlc_expiry_delta: old_request.final_tlc_expiry_delta,
        tlc_expiry_limit: old_request.tlc_expiry_limit,
        timeout: old_request.timeout,
        max_fee_amount: old_request.max_fee_amount,
        max_parts: old_request.max_parts,
        keysend: old_request.keysend,
        udt_type_script: old_request.udt_type_script,
        preimage: convert(old_request.preimage),
        allow_self_payment: old_request.allow_self_payment,
        dry_run: old_request.dry_run,
        custom_records: None, // Old version doesn't have this field
        hop_hints: vec![],    // New field
        router: vec![],       // New field
    };

    NewPaymentSession {
        request: request,
        retried_times: old.retried_times,
        last_error: old.last_error,
        try_limit: old.try_limit,
        status: convert(old.status),
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        route: convert(old.route),
        session_key: old.session_key,
    }
}

fn migrate_payment_session_v050(old: OldPaymentSessionV050) -> NewPaymentSession {
    let old_request = old.request.clone();

    let request = NewSendPaymentData {
        target_pubkey: convert(old_request.target_pubkey),
        amount: old_request.amount,
        payment_hash: convert(old_request.payment_hash),
        invoice: old_request.invoice,
        final_tlc_expiry_delta: old_request.final_tlc_expiry_delta,
        tlc_expiry_limit: old_request.tlc_expiry_limit,
        timeout: old_request.timeout,
        max_fee_amount: old_request.max_fee_amount,
        max_parts: old_request.max_parts,
        keysend: old_request.keysend,
        udt_type_script: old_request.udt_type_script,
        preimage: convert(old_request.preimage),
        allow_self_payment: old_request.allow_self_payment,
        dry_run: old_request.dry_run,
        custom_records: convert(old_request.custom_records),
        hop_hints: convert(old_request.hop_hints),
        router: vec![], // New field
    };

    NewPaymentSession {
        request: request,
        retried_times: old.retried_times,
        last_error: old.last_error,
        try_limit: old.try_limit,
        status: convert(old.status),
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        route: convert(old.route),
        session_key: old.session_key,
    }
}
