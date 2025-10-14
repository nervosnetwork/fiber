use crate::{
    fiber::{channel::TlcInfo, payment::MppMode},
    invoice::CkbInvoice,
};
use tracing::debug;

/// Check if the invoice is fulfilled by the tlc set
///
/// # Arguments
///
/// * `invoice` - The invoice to check
/// * `tlc_set` - The tlc set to check
///
/// # Returns
///
/// * `true` if the invoice is fulfilled, `false` otherwise
pub fn is_invoice_fulfilled(invoice: &CkbInvoice, tlc_set: &[TlcInfo]) -> bool {
    let Some(first_tlc) = tlc_set.first() else {
        // no tlcs to settle
        return false;
    };

    if invoice.mpp_mode() == Some(MppMode::AtomicMpp) {
        return is_atomic_mpp_fulfilled(tlc_set);
    }

    // check if total_amount is enough
    let total_amount = first_tlc.total_amount.unwrap_or(first_tlc.amount);

    if total_amount < invoice.amount.unwrap_or_default() {
        return false;
    }

    // check if tlc set are fulfilled
    let total_tlc_amount = tlc_set.iter().map(|tlc| tlc.amount).sum::<u128>();
    debug!(
        "checking total_tlc_amount: {}, total_amount: {}",
        total_tlc_amount, total_amount
    );
    total_tlc_amount >= total_amount
}

pub fn is_atomic_mpp_fulfilled(tlc_set: &[TlcInfo]) -> bool {
    if tlc_set.is_empty() {
        return false;
    }
    let first_tlc = &tlc_set[0];
    let total_amount = first_tlc.total_amount.unwrap_or(first_tlc.amount);
    let total_tlc_amount = tlc_set.iter().map(|tlc| tlc.amount).sum::<u128>();
    debug!(
        "checking atomic mpp total_tlc_amount: {}, total_amount: {}",
        total_tlc_amount, total_amount
    );
    total_tlc_amount >= total_amount
}
