use fiber::store::Store;
use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250507010725";

use crate::util::convert;

pub use fiber_v031::invoice::Attribute as OldAttribute;
pub use fiber_v031::invoice::CkbInvoice as OldCkbInvoice;
pub use fiber_v031::invoice::InvoiceData as OldInvoiceData;
pub use fiber_v051::invoice::Attribute as NewAttribute;
pub use fiber_v051::invoice::CkbInvoice as NewCkbInvoice;
pub use fiber_v051::invoice::InvoiceData as NewInvoiceData;

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

        info!("migrate Ckb Invoice ...");
        const CKB_INVOICE_PREFIX: u8 = 32;
        let prefix = vec![CKB_INVOICE_PREFIX];
        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            if bincode::deserialize::<NewCkbInvoice>(&v).is_ok() {
                continue;
            }

            let old = bincode::deserialize::<OldCkbInvoice>(&v);
            if let Ok(old) = old {
                let new = migrate_ckb_invoice(old);
                new.check_signature()
                    .expect("check signature for new ckb invoice");
                let new_bytes = bincode::serialize(&new).expect("serialize to new ckb invoice");
                db.put(k, new_bytes);
            }
        }
        return Ok(db);
    }

    fn version(&self) -> &str {
        &self.version
    }
}

fn convert_data(old: &OldCkbInvoice) -> NewInvoiceData {
    let old_data = old.data.clone();
    let old_attrs: Vec<_> = old_data
        .attrs
        .iter()
        .map(|attr| match attr {
            OldAttribute::ExpiryTime(duration) => NewAttribute::ExpiryTime(*duration),
            OldAttribute::FinalHtlcTimeout(hash) => NewAttribute::FinalHtlcTimeout(*hash),
            OldAttribute::FinalHtlcMinimumExpiryDelta(v) => {
                NewAttribute::FinalHtlcMinimumExpiryDelta(*v)
            }
            OldAttribute::Description(v) => NewAttribute::Description(v.clone()),
            OldAttribute::FallbackAddr(v) => NewAttribute::FallbackAddr(v.clone()),
            OldAttribute::UdtScript(v) => NewAttribute::UdtScript(convert(v)),
            OldAttribute::PayeePublicKey(v) => NewAttribute::PayeePublicKey(v.clone()),
            OldAttribute::HashAlgorithm(v) => NewAttribute::HashAlgorithm(convert(v)),
            OldAttribute::Feature(v) => NewAttribute::Feature(*v),
        })
        .collect();

    NewInvoiceData {
        timestamp: old_data.timestamp,
        payment_hash: convert(old_data.payment_hash),
        attrs: old_attrs,
    }
}

fn migrate_ckb_invoice(old: OldCkbInvoice) -> NewCkbInvoice {
    NewCkbInvoice {
        currency: convert(old.currency),
        amount: old.amount,
        signature: None, // we can not re-compute the signature here since we need secret_key, set it to None
        data: convert_data(&old),
    }
}
