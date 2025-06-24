use crate::{fiber::channel::TlcInfo, invoice::CkbInvoice};
use tracing::{debug, error};

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

    // check if total_amount is enough
    let total_amount = first_tlc.total_amount.unwrap_or(first_tlc.amount);

    if total_amount < invoice.amount.unwrap_or_default() {
        error!("total_amount is less than invoice amount");
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
