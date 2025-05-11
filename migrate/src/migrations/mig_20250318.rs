use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250318031910";

use crate::util::convert;
pub use fiber_v040::fiber::channel::ChannelActorState as ChannelActorStateV040;
pub use fiber_v040::fiber::channel::{
    ChannelTlcInfo as ChannelTlcInfoV040, PendingTlcs as PendingTlcsV040,
    PublicChannelInfo as PublicChannelInfoV040, RetryableTlcOperation as RetryableTlcOperationV040,
    TlcInfo as TlcInfoV040, TlcState as TlcStateV040,
};
pub use fiber_v040::fiber::types::PaymentHopData as PaymentHopDataV040;
pub use fiber_v040::fiber::types::PeeledOnionPacket as PeeledOnionPacketV040;

pub use fiber_v041::fiber::channel::ChannelActorState as ChannelActorStateV041;
pub use fiber_v041::fiber::channel::{
    ChannelTlcInfo as ChannelTlcInfoV041, PendingTlcs as PendingTlcsV041,
    PublicChannelInfo as PublicChannelInfoV041, RetryableTlcOperation as RetryableTlcOperationV041,
    TlcInfo as TlcInfoV041, TlcState as TlcStateV041,
};
pub use fiber_v041::fiber::types::PaymentHopData as PaymentHopDataV041;
pub use fiber_v041::fiber::types::PeeledOnionPacket as PeeledOnionPacketV041;

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

fn convert_retryable_tlc_operations(
    old: Vec<RetryableTlcOperationV040>,
) -> Vec<RetryableTlcOperationV041> {
    old.into_iter().map(|op| convert_retryable_op(op)).collect()
}

fn convert_hop_data(old: PaymentHopDataV040) -> PaymentHopDataV041 {
    PaymentHopDataV041 {
        amount: old.amount,
        expiry: old.expiry,
        payment_preimage: convert(old.payment_preimage),
        hash_algorithm: convert(old.hash_algorithm),
        funding_tx_hash: convert(old.funding_tx_hash),
        next_hop: convert(old.next_hop),
        // This is the new field in v041
        custom_records: None,
    }
}

fn convert_packet(
    old: PeeledOnionPacketV040<PaymentHopDataV040>,
) -> PeeledOnionPacketV041<PaymentHopDataV041> {
    PeeledOnionPacketV041::<PaymentHopDataV041> {
        current: convert_hop_data(old.current),
        shared_secret: convert(old.shared_secret),
        next: convert(old.next),
    }
}

fn convert_retryable_op(old: RetryableTlcOperationV040) -> RetryableTlcOperationV041 {
    match old {
        RetryableTlcOperationV040::RemoveTlc(..) => convert(old),
        RetryableTlcOperationV040::RelayRemoveTlc(..) => convert(old),
        RetryableTlcOperationV040::ForwardTlc(channel_id, tlc_id, onion_packet, fee, is_retry) => {
            RetryableTlcOperationV041::ForwardTlc(
                convert(channel_id),
                convert(tlc_id),
                convert_packet(onion_packet),
                fee,
                is_retry,
            )
        }
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

        const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.clone().as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            if let Ok(_) = bincode::deserialize::<ChannelActorStateV041>(&v) {
                // if we can deserialize the data correctly with new version, just skip it.
                continue;
            }

            let old_channel_state: ChannelActorStateV040 =
                bincode::deserialize(&v).expect("deserialize to old channel state");

            let old_tlc_state = old_channel_state.tlc_state.clone();
            let new_tlc_state = TlcStateV041 {
                offered_tlcs: convert(old_tlc_state.offered_tlcs),
                received_tlcs: convert(old_tlc_state.received_tlcs),
                retryable_tlc_operations: convert_retryable_tlc_operations(
                    old_tlc_state.retryable_tlc_operations,
                ),
                applied_add_tlcs: convert(old_tlc_state.applied_add_tlcs),
                applied_remove_tlcs: convert(old_tlc_state.applied_remove_tlcs),
                waiting_ack: old_tlc_state.waiting_ack,
            };

            let new_channel_state = ChannelActorStateV041 {
                state: convert(old_channel_state.state),
                local_pubkey: convert(old_channel_state.local_pubkey),
                remote_pubkey: convert(old_channel_state.remote_pubkey),
                id: convert(old_channel_state.id),
                funding_tx: old_channel_state.funding_tx,
                funding_tx_confirmed_at: old_channel_state.funding_tx_confirmed_at,
                funding_udt_type_script: old_channel_state.funding_udt_type_script,
                is_acceptor: old_channel_state.is_acceptor,
                to_local_amount: old_channel_state.to_local_amount,
                to_remote_amount: old_channel_state.to_remote_amount,
                local_reserved_ckb_amount: old_channel_state.local_reserved_ckb_amount,
                remote_reserved_ckb_amount: old_channel_state.remote_reserved_ckb_amount,
                commitment_fee_rate: old_channel_state.commitment_fee_rate,
                commitment_delay_epoch: old_channel_state.commitment_delay_epoch,
                funding_fee_rate: old_channel_state.funding_fee_rate,
                signer: convert(old_channel_state.signer),
                local_channel_public_keys: convert(old_channel_state.local_channel_public_keys),
                commitment_numbers: convert(old_channel_state.commitment_numbers),
                local_constraints: convert(old_channel_state.local_constraints),
                remote_constraints: convert(old_channel_state.remote_constraints),
                tlc_state: new_tlc_state,
                remote_shutdown_script: old_channel_state.remote_shutdown_script,
                local_shutdown_script: old_channel_state.local_shutdown_script,
                last_commitment_signed_remote_nonce: old_channel_state
                    .last_commitment_signed_remote_nonce,
                last_revoke_and_ack_remote_nonce: old_channel_state
                    .last_revoke_and_ack_remote_nonce,
                last_committed_remote_nonce: old_channel_state.last_committed_remote_nonce,
                latest_commitment_transaction: old_channel_state.latest_commitment_transaction,
                remote_commitment_points: convert(old_channel_state.remote_commitment_points),
                remote_channel_public_keys: convert(old_channel_state.remote_channel_public_keys),
                local_shutdown_info: convert(old_channel_state.local_shutdown_info),
                remote_shutdown_info: convert(old_channel_state.remote_shutdown_info),
                reestablishing: old_channel_state.reestablishing,
                created_at: old_channel_state.created_at,
                public_channel_info: convert(old_channel_state.public_channel_info),
                local_tlc_info: convert(old_channel_state.local_tlc_info),
                remote_tlc_info: convert(old_channel_state.remote_tlc_info),
                network: None,
                scheduled_channel_update_handle: None,
                waiting_peer_response: None,
            };

            let new_channel_state_bytes =
                bincode::serialize(&new_channel_state).expect("serialize to new channel state");

            db.put(k, new_channel_state_bytes);
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
