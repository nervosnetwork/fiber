use bincode::Options;
use ckb_types::prelude::Entity;
use fiber_types::{Attempt, PaymentSession, PaymentStatus};

use crate::migration::{Migration, MigrationStore, MIGRATION_VERSION_KEY};

use super::decode_as_new;

const MIGRATION_DB_VERSION: &str = "20261111120000";
const PAYMENT_SESSION_PREFIX: &[u8] = &[0xC0];

fn recover_stopped_session(
    store: &dyn MigrationStore,
    mut session: PaymentSession,
) -> Result<(PaymentSession, Vec<Attempt>), String> {
    if session.status.is_final() {
        return Ok((session, Vec::new()));
    }
    let prefix = [
        &[fiber_types::schema::ATTEMPT_PREFIX],
        session.payment_hash().as_ref(),
    ]
    .concat();
    let attempts = store
        .iter_prefix(&prefix)
        .map(|(_, bytes)| decode_exact::<Attempt>(&bytes))
        .collect::<Result<Vec<_>, _>>()?;
    // Older payers could derive Failed from exhausted attempts and retire their
    // indexes without persisting the aggregate session. Recover the stop intent,
    // but let the current payer check channel evidence before publishing Failed.
    if !attempts.is_empty()
        && attempts.iter().all(Attempt::is_failed)
        && attempts
            .iter()
            .map(|attempt| u64::from(attempt.tried_times))
            .sum::<u64>()
            >= u64::from(session.try_limit)
    {
        session.retry_stopped = true;
        session.status = PaymentStatus::Inflight;
        return Ok((session, attempts));
    }
    Ok((session, Vec::new()))
}

fn decode_exact<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .deserialize(bytes)
        .map_err(|error| error.to_string())
}

fn convert(bytes: &[u8]) -> Result<PaymentSession, String> {
    if let Ok(session) = decode_exact::<PaymentSession>(bytes) {
        return Ok(session);
    }
    let old: fiber_types_0100::PaymentSession = decode_exact(bytes)?;
    let status: PaymentStatus = decode_as_new(old.status)?;
    Ok(PaymentSession {
        request: decode_as_new(old.request)?,
        last_error: old.last_error,
        last_error_code: decode_as_new(old.last_error_code)?,
        try_limit: old.try_limit,
        status,
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        cached_attempts: Vec::new(),
        // Old Failed sessions may still own TLCs. Recovery must not send more
        // value while it waits for their actual outcomes.
        retry_stopped: status == PaymentStatus::Failed,
    })
}

#[derive(Default)]
pub struct MigrationObj;

impl MigrationObj {
    pub fn new() -> Self {
        Self
    }
}

impl Migration for MigrationObj {
    fn version(&self) -> &str {
        MIGRATION_DB_VERSION
    }

    fn preflight(&self, store: &dyn MigrationStore) -> Result<(), String> {
        // The registry runs every pending preflight before any migration. The
        // earlier trampoline-context migration owns this older released layout.
        let trampoline_migration_pending = store
            .get(MIGRATION_VERSION_KEY)
            .is_none_or(|version| version.as_slice() < b"20260618120000".as_slice());
        for (_, bytes) in store.iter_prefix(PAYMENT_SESSION_PREFIX) {
            if trampoline_migration_pending
                && decode_exact::<fiber_types_081::PaymentSession>(&bytes).is_ok()
            {
                continue;
            }
            let session = convert(&bytes).map_err(|error| {
                format!("Invalid PaymentSession before retry-stopped migration: {error}")
            })?;
            recover_stopped_session(store, session)?;
        }
        Ok(())
    }

    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        // A bounded scan is needed once at upgrade, never on ordinary startup.
        self.preflight(store)?;
        for (key, bytes) in store.iter_prefix(PAYMENT_SESSION_PREFIX) {
            let (session, recover_attempts) = recover_stopped_session(store, convert(&bytes)?)?;
            let encoded = bincode::serialize(&session).map_err(|error| error.to_string())?;
            if encoded != bytes {
                store.put(&key, &encoded);
            }
            for attempt in recover_attempts {
                if let Some(outpoint) = attempt.first_hop_channel_outpoint() {
                    let key = [
                        &[fiber_types::schema::ATTEMPT_CHANNEL_INDEX_PREFIX],
                        outpoint.as_slice(),
                        session.payment_hash().as_ref(),
                        &attempt.id.to_le_bytes(),
                    ]
                    .concat();
                    store.put(&key, &[]);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::StorageBackend;
    use crate::migration::{INIT_DB_VERSION, MIGRATION_VERSION_KEY};
    #[test]
    fn test_payment_retry_stop_migration_chains_from_released_trampoline_layout() {
        use fiber_types_081::{PaymentSession, PaymentStatus, SendPaymentData, TrampolineContext};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let hash = fiber_types::Hash256::from([42; 32]);
        let old = PaymentSession {
            request: SendPaymentData {
                target_pubkey: bincode::deserialize(
                    &bincode::serialize(&fiber_types::Privkey::from_slice(&[1; 32]).pubkey())
                        .unwrap(),
                )
                .unwrap(),
                amount: 1000,
                payment_hash: bincode::deserialize(&bincode::serialize(&hash).unwrap()).unwrap(),
                invoice: None,
                final_tlc_expiry_delta: 40,
                tlc_expiry_limit: 2016,
                timeout: Some(60),
                max_fee_amount: Some(100),
                max_parts: None,
                keysend: false,
                udt_type_script: None,
                preimage: None,
                custom_records: None,
                allow_self_payment: false,
                hop_hints: vec![],
                router: vec![],
                allow_mpp: false,
                dry_run: false,
                trampoline_hops: None,
                trampoline_context: Some(TrampolineContext {
                    remaining_trampoline_onion: vec![1, 2, 3],
                    previous_tlcs: vec![],
                    hash_algorithm: fiber_types_081::HashAlgorithm::Sha256,
                }),
            },
            last_error: None,
            last_error_code: None,
            try_limit: 3,
            status: PaymentStatus::Inflight,
            created_at: 1000,
            last_updated_at: 1000,
            cached_attempts: Vec::new(),
        };
        {
            let store = crate::Store::open_db(path).unwrap();
            store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
            store.put(
                [&[0xC0], hash.as_ref()].concat(),
                bincode::serialize(&old).unwrap(),
            );
        }
        let store = crate::Store::open_db(path).unwrap();
        let mut migrations = crate::db_migrate::DbMigrate::new();
        crate::migrations::register_all_migrations(&mut migrations);
        assert!(migrations
            .auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}))
            .is_ok());
        let session: fiber_types::PaymentSession =
            bincode::deserialize(&store.get([&[0xC0], hash.as_ref()].concat()).unwrap()).unwrap();
        assert!(!session.retry_stopped);
        assert_eq!(session.status, fiber_types::PaymentStatus::Inflight);
        let context = session.request.trampoline_context.unwrap();
        assert_eq!(context.remaining_trampoline_onion, vec![1, 2, 3]);
        assert!(context.max_outgoing_tlc_expiry.is_none());
    }
}
