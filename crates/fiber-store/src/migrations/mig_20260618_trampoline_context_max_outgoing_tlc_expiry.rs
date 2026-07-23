use super::decode_as_new;
use crate::migration::{Migration, MigrationStore};
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260618120000";

const PAYMENT_SESSION_PREFIX: &[u8] = &[0xC0];

pub use fiber_types_081::payment::PaymentSession as OldPaymentSession;
pub use fiber_types_090::payment::PaymentSession as NewPaymentSession;

type NewSendPaymentData = fiber_types_090::payment::SendPaymentData;
type NewTrampolineContext = fiber_types_090::payment::TrampolineContext;
type NewHash256 = fiber_types_090::Hash256;
type NewPaymentCustomRecords = fiber_types_090::payment::PaymentCustomRecords;
type NewHopHint = fiber_types_090::payment::HopHint;
type NewPubkey = fiber_types_090::Pubkey;
type NewRouterHop = fiber_types_090::payment::RouterHop;
type NewTlcErrorCode = fiber_types_090::payment::TlcErrorCode;
type NewPaymentStatus = fiber_types_090::payment::PaymentStatus;

fn convert_payment_session(old: OldPaymentSession) -> Result<NewPaymentSession, String> {
    let trampoline_context = old
        .request
        .trampoline_context
        .map(|ctx| -> Result<NewTrampolineContext, String> {
            Ok(NewTrampolineContext {
                remaining_trampoline_onion: ctx.remaining_trampoline_onion,
                previous_tlcs: decode_as_new(ctx.previous_tlcs)?,
                hash_algorithm: decode_as_new(ctx.hash_algorithm)?,
                max_outgoing_tlc_expiry: None,
            })
        })
        .transpose()?;

    Ok(NewPaymentSession {
        request: NewSendPaymentData {
            target_pubkey: decode_as_new(old.request.target_pubkey)?,
            amount: old.request.amount,
            payment_hash: decode_as_new::<_, NewHash256>(old.request.payment_hash)?,
            invoice: old.request.invoice,
            final_tlc_expiry_delta: old.request.final_tlc_expiry_delta,
            tlc_expiry_limit: old.request.tlc_expiry_limit,
            timeout: old.request.timeout,
            max_fee_amount: old.request.max_fee_amount,
            max_parts: old.request.max_parts,
            keysend: old.request.keysend,
            udt_type_script: old.request.udt_type_script,
            preimage: old
                .request
                .preimage
                .map(decode_as_new::<_, NewHash256>)
                .transpose()?,
            custom_records: old
                .request
                .custom_records
                .map(decode_as_new::<_, NewPaymentCustomRecords>)
                .transpose()?,
            allow_self_payment: old.request.allow_self_payment,
            hop_hints: old
                .request
                .hop_hints
                .into_iter()
                .map(decode_as_new::<_, NewHopHint>)
                .collect::<Result<Vec<_>, _>>()?,
            router: old
                .request
                .router
                .into_iter()
                .map(decode_as_new::<_, NewRouterHop>)
                .collect::<Result<Vec<_>, _>>()?,
            allow_mpp: old.request.allow_mpp,
            dry_run: old.request.dry_run,
            trampoline_hops: old
                .request
                .trampoline_hops
                .map(|hops| {
                    hops.into_iter()
                        .map(decode_as_new::<_, NewPubkey>)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?,
            trampoline_context,
        },
        last_error: old.last_error,
        last_error_code: old
            .last_error_code
            .map(decode_as_new::<_, NewTlcErrorCode>)
            .transpose()?,
        try_limit: old.try_limit,
        status: decode_as_new::<_, NewPaymentStatus>(old.status)?,
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        cached_attempts: Vec::new(),
    })
}

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
    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        info!(
            "Migrating to {}: adding max_outgoing_tlc_expiry to TrampolineContext in PaymentSession ...",
            MIGRATION_DB_VERSION
        );

        let entries = store.collect_prefix(PAYMENT_SESSION_PREFIX);
        let total = entries.len();
        let mut migrated = 0u64;
        let mut skipped = 0u64;

        for (key, value) in entries {
            if bincode::deserialize::<NewPaymentSession>(&value).is_ok() {
                skipped += 1;
                continue;
            }

            let old: OldPaymentSession = bincode::deserialize(&value).map_err(|e| {
                format!("Failed to deserialize old PaymentSession: {e}")
            })?;

            let new = convert_payment_session(old)?;

            let new_bytes = bincode::serialize(&new).map_err(|e| {
                format!("Failed to serialize new PaymentSession: {e}")
            })?;

            store.put(&key, &new_bytes);
            migrated += 1;
        }

        info!(
            "Migration {} complete: {} migrated, {} skipped ({} total)",
            MIGRATION_DB_VERSION, migrated, skipped, total
        );
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

#[cfg(test)]
mod tests {
    use super::{Migration, MigrationObj, NewPaymentSession, OldPaymentSession};
    use crate::backend::StorageBackend;
    use fiber_types_081::payment::*;
    use fiber_types_081::sample::StoreSample;

    const PAYMENT_SESSION_PREFIX_BYTE: u8 = 0xC0;

    fn gen_store() -> crate::Store {
        let tmp_dir = tempfile::Builder::new()
            .prefix("test-trampoline-context-migration")
            .tempdir()
            .unwrap();
        let path = tmp_dir.as_ref().to_path_buf();
        crate::Store::open_db(&path).unwrap()
    }

    fn deterministic_pubkey() -> fiber_types_081::Pubkey {
        #[rustfmt::skip]
        let bytes: [u8; 33] = [
            0x02,
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac,
            0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b, 0x07,
            0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9,
            0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17, 0x98,
        ];
        fiber_types_081::Pubkey::from_slice(&bytes).unwrap()
    }

    fn make_old_payment_session(with_trampoline: bool) -> OldPaymentSession {
        let custom_records = PaymentCustomRecords::samples(42)
            .into_iter()
            .next()
            .unwrap();

        OldPaymentSession {
            request: SendPaymentData {
                target_pubkey: deterministic_pubkey(),
                amount: 1000,
                payment_hash: fiber_types_081::Hash256::default(),
                invoice: None,
                final_tlc_expiry_delta: 40,
                tlc_expiry_limit: 2016,
                timeout: Some(60),
                max_fee_amount: Some(100),
                max_parts: None,
                keysend: false,
                udt_type_script: None,
                preimage: None,
                custom_records: Some(custom_records),
                allow_self_payment: false,
                hop_hints: vec![],
                router: vec![],
                allow_mpp: false,
                dry_run: false,
                trampoline_hops: None,
                trampoline_context: if with_trampoline {
                    Some(TrampolineContext {
                        remaining_trampoline_onion: vec![1, 2, 3],
                        previous_tlcs: vec![],
                        hash_algorithm: fiber_types_081::HashAlgorithm::Sha256,
                    })
                } else {
                    None
                },
            },
            last_error: None,
            last_error_code: None,
            try_limit: 3,
            status: PaymentStatus::Inflight,
            created_at: 1000,
            last_updated_at: 1000,
            cached_attempts: Vec::new(),
        }
    }

    #[test]
    fn migrates_payment_session_with_trampoline_context() {
        let store = gen_store();
        let old = make_old_payment_session(true);
        let old_bytes = bincode::serialize(&old).expect("serialize old payment session");

        let key = vec![PAYMENT_SESSION_PREFIX_BYTE, 1, 2, 3];
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new()
            .migrate(&store)
            .expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewPaymentSession =
            bincode::deserialize(&new_bytes).expect("deserialize migrated payment session");

        assert!(new.request.trampoline_context.is_some());
        let ctx = new.request.trampoline_context.unwrap();
        assert_eq!(ctx.remaining_trampoline_onion, vec![1, 2, 3]);
        assert!(ctx.max_outgoing_tlc_expiry.is_none());
        assert_eq!(new.request.amount, 1000);
    }

    #[test]
    fn migrates_payment_session_without_trampoline_context() {
        let store = gen_store();
        let old = make_old_payment_session(false);
        let old_bytes = bincode::serialize(&old).expect("serialize old payment session");

        let key = vec![PAYMENT_SESSION_PREFIX_BYTE, 4, 5, 6];
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new()
            .migrate(&store)
            .expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewPaymentSession =
            bincode::deserialize(&new_bytes).expect("deserialize migrated payment session");

        assert!(new.request.trampoline_context.is_none());
    }
}
