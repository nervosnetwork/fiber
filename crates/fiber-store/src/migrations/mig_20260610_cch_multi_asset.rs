use crate::migration::{Migration, MigrationStore};
use serde::{de::DeserializeOwned, Serialize};
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260610120000";

/// Key prefix for persisted `CchOrder` entries (see `fiber_types::schema`).
const CCH_ORDER_PREFIX: &[u8] = &[232];

pub use fiber_types_081::cch::CchOrder as OldCchOrder;
pub use fiber_types_local::cch::CchOrder as NewCchOrder;

type NewHash256 = fiber_types_local::Hash256;

/// Convert a value serialized with the legacy (`fiber_types_081`) layout into
/// the current (`fiber_types_local`) layout by round-tripping through bincode.
///
/// This is safe for every field carried over by this migration:
///   - `Hash256` is a fixed 32-byte blob in both versions.
///   - `ckb_jsonrpc_types::Script` resolves to the same `ckb-jsonrpc-types` v1
///     serde layout in both crates.
///   - `CchInvoice` and `CchOrderStatus` keep identical bincode encodings
///     (the invoice variants serialize via `DisplayFromStr`, i.e. the stable
///     encoded-invoice string, and the status enum keeps variant order 0..=5).
fn decode_as_new<TOld, TNew>(value: TOld) -> Result<TNew, String>
where
    TOld: Serialize,
    TNew: DeserializeOwned,
{
    let bytes = bincode::serialize(&value)
        .map_err(|e| format!("Failed to serialize legacy CCH field: {}", e))?;
    bincode::deserialize(&bytes)
        .map_err(|e| format!("Failed to deserialize migrated CCH field: {}", e))
}

/// Reshape a single legacy single-asset `CchOrder` into the multi-asset layout.
///
/// Legacy orders always carried a wrapped-BTC type script and denominated both
/// legs in satoshis, so:
///   - `wrapped_btc_type_script` becomes `Some(fiber_type_script)`,
///   - BTC amounts shift sat -> msat (x1000), and
///   - the Fiber leg keeps the legacy 1 sat == 1 smallest-unit assumption.
fn convert_cch_order(old: OldCchOrder) -> Result<NewCchOrder, String> {
    Ok(NewCchOrder {
        created_at: old.created_at,
        expiry_delta_seconds: old.expiry_delta_seconds,
        fiber_type_script: Some(decode_as_new(old.wrapped_btc_type_script)?),
        outgoing_pay_req: old.outgoing_pay_req,
        incoming_invoice: decode_as_new(old.incoming_invoice)?,
        payment_hash: decode_as_new(old.payment_hash)?,
        payment_preimage: old
            .payment_preimage
            .map(decode_as_new::<_, NewHash256>)
            .transpose()?,
        lightning_invoice_amount: old
            .amount_sats
            .checked_mul(1000)
            .ok_or_else(|| "lightning_invoice_amount overflow during CCH migration".to_string())?,
        btc_fee_msat: old
            .fee_sats
            .checked_mul(1000)
            .ok_or_else(|| "btc_fee_msat overflow during CCH migration".to_string())?,
        fiber_invoice_amount: old.amount_sats,
        status: decode_as_new(old.status)?,
        failure_reason: old.failure_reason,
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
            "Migrating to {}: reshaping CchOrder for multi-asset swaps ...",
            MIGRATION_DB_VERSION
        );

        let entries = store.collect_prefix(CCH_ORDER_PREFIX);
        let total = entries.len();
        let mut migrated = 0u64;
        let mut skipped = 0u64;

        for (key, value) in entries {
            if bincode::deserialize::<NewCchOrder>(&value).is_ok() {
                skipped += 1;
                continue;
            }

            let old: OldCchOrder = bincode::deserialize(&value)
                .map_err(|e| format!("Failed to deserialize old CchOrder: {}", e))?;

            let new = convert_cch_order(old)?;

            let new_bytes = bincode::serialize(&new)
                .map_err(|e| format!("Failed to serialize new CchOrder: {}", e))?;

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
    use super::{convert_cch_order, Migration, MigrationObj, NewCchOrder, OldCchOrder};
    use crate::backend::StorageBackend;
    use fiber_types_081::cch::{CchInvoice as OldCchInvoice, CchOrderStatus as OldCchOrderStatus};
    use fiber_types_local::cch::CchInvoice as NewCchInvoice;

    const CCH_ORDER_PREFIX: u8 = 232;
    const PAYMENT_HASH: [u8; 32] = [0xAA; 32];

    fn gen_path() -> std::path::PathBuf {
        tempfile::Builder::new()
            .prefix("test-cch-multi-asset-migration")
            .tempdir()
            .unwrap()
            .as_ref()
            .to_path_buf()
    }

    /// Standard BOLT-11 spec example invoice (reused verbatim from the
    /// `fiber-lib` CCH tests). Parsing it via `CchInvoice`'s `DisplayFromStr`
    /// codec lets the migration test cover a real Lightning order without
    /// pulling `bitcoin`/`lightning-invoice` in just to mint one.
    const TEST_BOLT11_INVOICE: &str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap9us6v52vjjsrvywa6rt52cm9r9zqt8r2t7mlcwspyetp5h2tztugp9lfyql";

    fn build_old_order() -> OldCchOrder {
        OldCchOrder {
            created_at: 1_700_000_000,
            expiry_delta_seconds: 3_600,
            wrapped_btc_type_script: Default::default(),
            outgoing_pay_req: "lnbc-migration-test".to_string(),
            incoming_invoice: OldCchInvoice::Lightning(
                TEST_BOLT11_INVOICE.parse().expect("parse BOLT-11 invoice"),
            ),
            payment_hash: PAYMENT_HASH.into(),
            payment_preimage: Some([0xBB; 32].into()),
            amount_sats: 100_000,
            fee_sats: 250,
            status: OldCchOrderStatus::Pending,
            failure_reason: None,
        }
    }

    #[test]
    fn converts_legacy_fields() {
        let old = build_old_order();
        let old_bytes = bincode::serialize(&old).expect("serialize old CchOrder");

        // Legacy bytes must NOT be mistaken for already-migrated data.
        assert!(bincode::deserialize::<NewCchOrder>(&old_bytes).is_err());

        let decoded_old: OldCchOrder =
            bincode::deserialize(&old_bytes).expect("deserialize old CchOrder");
        let new = convert_cch_order(decoded_old).expect("convert CchOrder");

        let new_bytes = bincode::serialize(&new).expect("serialize new CchOrder");
        let new: NewCchOrder =
            bincode::deserialize(&new_bytes).expect("deserialize migrated CchOrder");

        assert_eq!(new.created_at, 1_700_000_000);
        assert_eq!(new.expiry_delta_seconds, 3_600);
        assert_eq!(new.outgoing_pay_req, "lnbc-migration-test");
        assert_eq!(new.lightning_invoice_amount, 100_000 * 1_000);
        assert_eq!(new.btc_fee_msat, 250 * 1_000);
        assert_eq!(new.fiber_invoice_amount, 100_000);
        assert!(new.fiber_type_script.is_some());
        assert!(matches!(new.incoming_invoice, NewCchInvoice::Lightning(_)));
    }

    #[test]
    fn migrates_order_in_store() {
        let store = crate::Store::open_db(&gen_path()).expect("open store");

        let old = build_old_order();
        let key = [&[CCH_ORDER_PREFIX], PAYMENT_HASH.as_slice()].concat();
        let old_bytes = bincode::serialize(&old).expect("serialize old CchOrder");
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new()
            .migrate(&store)
            .expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value present");
        let new: NewCchOrder =
            bincode::deserialize(&new_bytes).expect("deserialize migrated CchOrder");
        assert_eq!(new.lightning_invoice_amount, 100_000 * 1_000);
        assert!(new.fiber_type_script.is_some());

        // Idempotency: re-running must skip the already-migrated entry.
        MigrationObj::new()
            .migrate(&store)
            .expect("second migration should succeed");
        let again = StorageBackend::get(&store, &key).expect("value still present");
        assert_eq!(again, new_bytes);
    }
}
