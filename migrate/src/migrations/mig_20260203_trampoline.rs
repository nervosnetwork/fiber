use crate::util::convert;
use fiber_v061::fiber::channel::{
    ChannelActorState as OldChannelActorState, TlcInfo as OldTlcInfo, TlcState as OldTlcState,
};
use fiber_v061::fiber::payment::PaymentSession as OldPaymentSession;
use fiber_v061::fiber::payment::SendPaymentData as OldSendPaymentData;
use fiber_v070::{
    fiber::channel::{
        ChannelActorState as NewChannelActorState, PendingTlcs as NewPendingTlcs,
        TlcInfo as NewTlcInfo, TlcState as NewTlcState,
    },
    fiber::payment::{PaymentSession as NewPaymentSession, SendPaymentData as NewSendPaymentData},
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

// Remember to update the version number here, sample `20311116135521`
const MIGRATION_DB_VERSION: &str = "20260203152333";

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
                continue;
            }

            let old: OldPaymentSession =
                bincode::deserialize(&v).expect("deserialize to old payment session");
            let new = migrate_payment_session(old);

            let new_bytes = bincode::serialize(&new).expect("serialize to new payment session");
            db.put(k, new_bytes);
        }

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}

fn migrate_channel_state(old: OldChannelActorState) -> NewChannelActorState {
    let state = convert(old.state);
    let local_pubkey = convert(old.local_pubkey);
    let remote_pubkey = convert(old.remote_pubkey);
    let id = convert(old.id);
    let signer = convert(old.signer);
    let local_channel_public_keys = convert(old.local_channel_public_keys);
    let commitment_numbers = convert(old.commitment_numbers);
    let local_constraints = convert(old.local_constraints);
    let remote_constraints = convert(old.remote_constraints);
    let tlc_state = migrate_tlc_state(old.tlc_state);
    let retryable_tlc_operations = convert(old.retryable_tlc_operations);
    let shutdown_transaction_hash = convert(old.shutdown_transaction_hash);
    let waiting_forward_tlc_tasks = convert(old.waiting_forward_tlc_tasks);
    let remote_commitment_points = convert(old.remote_commitment_points);
    let remote_channel_public_keys = convert(old.remote_channel_public_keys);
    let local_shutdown_info = convert(old.local_shutdown_info);
    let remote_shutdown_info = convert(old.remote_shutdown_info);
    let last_revoke_ack_msg = convert(old.last_revoke_ack_msg);
    let public_channel_info = convert(old.public_channel_info);
    let local_tlc_info = convert(old.local_tlc_info);
    let remote_tlc_info = convert(old.remote_tlc_info);
    NewChannelActorState {
        state,
        local_pubkey,
        remote_pubkey,
        id,
        funding_tx: old.funding_tx,
        funding_tx_confirmed_at: old.funding_tx_confirmed_at,
        funding_udt_type_script: old.funding_udt_type_script,
        is_acceptor: old.is_acceptor,
        // New field: default to false for existing channels
        is_one_way: false,
        to_local_amount: old.to_local_amount,
        to_remote_amount: old.to_remote_amount,
        local_reserved_ckb_amount: old.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: old.remote_reserved_ckb_amount,
        commitment_fee_rate: old.commitment_fee_rate,
        commitment_delay_epoch: old.commitment_delay_epoch,
        funding_fee_rate: old.funding_fee_rate,
        signer,
        local_channel_public_keys,
        commitment_numbers,
        local_constraints,
        remote_constraints,
        tlc_state,
        remote_shutdown_script: old.remote_shutdown_script,
        local_shutdown_script: old.local_shutdown_script,
        last_committed_remote_nonce: old.last_committed_remote_nonce,
        remote_revocation_nonce_for_next: old.remote_revocation_nonce_for_next,
        remote_revocation_nonce_for_send: old.remote_revocation_nonce_for_send,
        remote_revocation_nonce_for_verify: old.remote_revocation_nonce_for_verify,
        retryable_tlc_operations,
        shutdown_transaction_hash,
        waiting_forward_tlc_tasks,
        latest_commitment_transaction: old.latest_commitment_transaction,
        remote_commitment_points,
        remote_channel_public_keys,
        local_shutdown_info,
        remote_shutdown_info,
        reestablishing: old.reestablishing,
        last_revoke_ack_msg,
        created_at: old.created_at,
        public_channel_info,
        local_tlc_info,
        remote_tlc_info,
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
        // New field: default to false for existing TLCs
        is_trampoline_hop: false,
        created_at: convert(old.created_at),
        removed_reason: convert(old.removed_reason),
        forwarding_tlc: old
            .forwarding_tlc
            .map(|(channel_id, tlc_id)| (convert(channel_id), tlc_id)),
        removed_confirmed_at: old.removed_confirmed_at,
        applied_flags: convert(old.applied_flags),
    }
}

fn migrate_payment_session(old: OldPaymentSession) -> NewPaymentSession {
    NewPaymentSession {
        request: migrate_send_payment_data(old.request),
        last_error: old.last_error,
        last_error_code: None,
        try_limit: old.try_limit,
        status: convert(old.status),
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        cached_attempts: vec![],
    }
}

fn migrate_send_payment_data(old: OldSendPaymentData) -> NewSendPaymentData {
    NewSendPaymentData {
        target_pubkey: convert(old.target_pubkey),
        amount: old.amount,
        payment_hash: convert(old.payment_hash),
        invoice: old.invoice,
        final_tlc_expiry_delta: old.final_tlc_expiry_delta,
        tlc_expiry_limit: old.tlc_expiry_limit,
        timeout: old.timeout,
        max_fee_amount: old.max_fee_amount,
        max_parts: old.max_parts,
        keysend: old.keysend,
        udt_type_script: old.udt_type_script,
        preimage: convert(old.preimage),
        custom_records: convert(old.custom_records),
        allow_self_payment: old.allow_self_payment,
        hop_hints: convert(old.hop_hints),
        router: convert(old.router),
        allow_mpp: old.allow_mpp,
        dry_run: old.dry_run,
        // New fields: default to None for existing payments
        trampoline_hops: None,
        trampoline_context: None,
        channel_stats: Default::default(),
    }
}
