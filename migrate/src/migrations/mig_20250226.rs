use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::ops::Iterate;
use rocksdb::ops::Put;
use rocksdb::DB;
use std::sync::Arc;
use tracing::info;

use crate::util::convert;
use {fiber_v032 as fiber_old, fiber_v033 as fiber_new};

const MIGRATION_DB_VERSION: &str = "20250226090023";

pub use fiber_new::fiber::graph::PaymentSession as NewPaymentSession;
pub use fiber_new::fiber::network::SendPaymentData as NewSendPaymentData;
pub use fiber_old::fiber::graph::PaymentSession as OldPaymentSession;
pub use fiber_old::fiber::network::SendPaymentData as OldSendPaymentData;

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
    fn migrate(
        &self,
        db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error> {
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        const PAYMENT_SESSION_PREFIX: u8 = 192;
        let prefix = vec![PAYMENT_SESSION_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            let old_payment_session: OldPaymentSession =
                bincode::deserialize(&v).expect("deserialize to old channel state");

            let old_request = old_payment_session.request.clone();

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
                // The meaning of hop_hints changed, we are dropping previous hop hints.
                hop_hints: vec![],
            };

            let new_payment_session = NewPaymentSession {
                request: request,
                retried_times: old_payment_session.retried_times,
                last_error: old_payment_session.last_error,
                try_limit: old_payment_session.try_limit,
                status: convert(old_payment_session.status),
                created_at: old_payment_session.created_at,
                last_updated_at: old_payment_session.last_updated_at,
                route: convert(old_payment_session.route),
                session_key: old_payment_session.session_key,
            };

            let new_payment_session_bytes =
                bincode::serialize(&new_payment_session).expect("serialize to new channel state");

            db.put(k, new_payment_session_bytes)
                .expect("save new channel state");
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
