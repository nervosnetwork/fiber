//! Scope hosted payments without changing ordinary payment keys or hashlocks.
use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};

use crate::migration::{Migration, MigrationStore};

use super::decode_as_new;

const MIGRATION_DB_VERSION: &str = "20261113000000";

/// The sole layout change in these leaf records is a trailing optional execution
/// ID. Explicitly append its None discriminant: serde(default) cannot supply
/// missing positional fields in bincode records.
pub(super) fn append_execution_scope<T: Serialize, U: DeserializeOwned>(
    old: T,
) -> Result<U, String> {
    let mut bytes = bincode::serialize(&old).map_err(|e| e.to_string())?;
    bytes.push(0);
    strict_decode(&bytes)
}

fn strict_decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .deserialize(bytes)
        .map_err(|e| e.to_string())
}

fn convert_session(
    old: fiber_types_0100::payment::PaymentSession,
) -> Result<fiber_types_current::payment::PaymentSession, String> {
    Ok(fiber_types_current::payment::PaymentSession {
        request: append_execution_scope(old.request)?,
        last_error: old.last_error,
        last_error_code: decode_as_new(old.last_error_code)?,
        try_limit: old.try_limit,
        status: decode_as_new(old.status)?,
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        cached_attempts: Vec::new(),
    })
}

pub struct MigrationObj;

impl MigrationObj {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MigrationObj {
    fn default() -> Self {
        Self::new()
    }
}

impl Migration for MigrationObj {
    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        for (key, bytes) in
            store.collect_prefix(&[fiber_types_current::schema::PAYMENT_SESSION_PREFIX])
        {
            if strict_decode::<fiber_types_current::payment::PaymentSession>(&bytes).is_ok() {
                continue;
            }
            let old = strict_decode::<fiber_types_0100::payment::PaymentSession>(&bytes)?;
            let new = convert_session(old)?;
            store.put(&key, &bincode::serialize(&new).map_err(|e| e.to_string())?);
        }
        for (key, bytes) in store.collect_prefix(&[fiber_types_current::schema::ATTEMPT_PREFIX]) {
            if strict_decode::<fiber_types_current::payment::Attempt>(&bytes).is_ok() {
                continue;
            }
            let old = strict_decode::<fiber_types_0100::payment::Attempt>(&bytes)?;
            let new: fiber_types_current::payment::Attempt = append_execution_scope(old)?;
            store.put(&key, &bincode::serialize(&new).map_err(|e| e.to_string())?);
        }
        // Channel TLCs and queued AddTlc operations are upgraded by the existing
        // pre-LSP channel migration, which already targets the current schema.
        Ok(())
    }

    fn version(&self) -> &str {
        MIGRATION_DB_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::StorageBackend;
    use fiber_types_0100::{
        channel::ChannelActorData, payment::*, sample::StoreSample, HashAlgorithm,
    };

    fn old_session() -> PaymentSession {
        let channel = ChannelActorData::samples(42).remove(0);
        PaymentSession {
            request: SendPaymentData {
                target_pubkey: channel.remote_pubkey,
                amount: 1000,
                payment_hash: [7; 32].into(),
                invoice: None,
                final_tlc_expiry_delta: 40,
                tlc_expiry_limit: 2016,
                timeout: Some(60),
                max_fee_amount: Some(100),
                max_parts: Some(2),
                keysend: false,
                udt_type_script: None,
                preimage: None,
                custom_records: None,
                allow_self_payment: false,
                hop_hints: vec![],
                router: vec![],
                allow_mpp: true,
                dry_run: false,
                trampoline_hops: None,
                trampoline_context: Some(TrampolineContext {
                    remaining_trampoline_onion: vec![1, 2, 3],
                    previous_tlcs: vec![],
                    hash_algorithm: HashAlgorithm::Sha256,
                    max_outgoing_tlc_expiry: Some(123456),
                }),
            },
            last_error: Some("retrying".into()),
            last_error_code: Some(fiber_types_0100::TlcErrorCode::TemporaryNodeFailure),
            try_limit: 3,
            status: PaymentStatus::Inflight,
            created_at: 1000,
            last_updated_at: 2000,
            cached_attempts: vec![],
        }
    }

    #[test]
    fn migrates_sessions_and_attempts_preserving_keys_and_retry_state() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::Store::open_db(root.path()).unwrap();
        let session = old_session();
        let mut attempt = session.new_attempt(
            1,
            session.request.target_pubkey,
            session.request.target_pubkey,
            vec![],
        );
        attempt.status = AttemptStatus::Retrying;
        attempt.tried_times = 2;
        attempt.last_error = Some("channel temporarily unavailable".into());
        let session_key = [&[0xC0][..], session.request.payment_hash.as_ref()].concat();
        let attempt_key = [
            &[0xC3][..],
            session.request.payment_hash.as_ref(),
            &1u64.to_le_bytes(),
        ]
        .concat();
        StorageBackend::put(&store, &session_key, bincode::serialize(&session).unwrap());
        StorageBackend::put(&store, &attempt_key, bincode::serialize(&attempt).unwrap());
        MigrationObj.migrate(&store).unwrap();
        let session_bytes = StorageBackend::get(&store, &session_key).unwrap();
        let attempt_bytes = StorageBackend::get(&store, &attempt_key).unwrap();
        let new: fiber_types_current::PaymentSession = strict_decode(&session_bytes).unwrap();
        assert!(new.request.execution_id.is_none());
        assert_eq!(new.try_limit, session.try_limit);
        assert_eq!(new.last_error, session.last_error);
        assert_eq!(new.created_at, session.created_at);
        assert_eq!(new.last_updated_at, session.last_updated_at);
        assert_eq!(
            bincode::serialize(&new.status).unwrap(),
            bincode::serialize(&session.status).unwrap()
        );
        let mut request_bytes = bincode::serialize(&new.request).unwrap();
        assert_eq!(request_bytes.pop(), Some(0));
        assert_eq!(request_bytes, bincode::serialize(&session.request).unwrap());
        let new_attempt: fiber_types_current::Attempt = strict_decode(&attempt_bytes).unwrap();
        assert!(new_attempt.execution_id.is_none());
        let mut expected_attempt = bincode::serialize(&attempt).unwrap();
        expected_attempt.push(0);
        assert_eq!(attempt_bytes, expected_attempt);
        MigrationObj.migrate(&store).unwrap();
        assert_eq!(
            StorageBackend::get(&store, &session_key).unwrap(),
            session_bytes
        );
        assert_eq!(
            StorageBackend::get(&store, &attempt_key).unwrap(),
            attempt_bytes
        );
    }

    #[test]
    fn rejects_corrupt_payment_records_without_overwriting_them() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::Store::open_db(root.path()).unwrap();
        let key = [0xC0, 1];
        StorageBackend::put(&store, key, [1, 2]);
        assert!(MigrationObj.migrate(&store).is_err());
        assert_eq!(StorageBackend::get(&store, key).unwrap(), vec![1, 2]);
    }
}
