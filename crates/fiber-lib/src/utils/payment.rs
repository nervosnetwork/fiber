use crate::{fiber::channel::TlcInfo, invoice::CkbInvoice};
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
pub fn is_invoice_fulfilled<'a, I>(invoice: &CkbInvoice, tlcs: I) -> bool
where
    I: IntoIterator<Item = &'a TlcInfo>,
{
    let mut it = tlcs.into_iter();
    let Some(first_tlc) = it.next() else {
        return false;
    };

    // check if total_amount is enough
    let total_amount = first_tlc.total_amount.unwrap_or(first_tlc.amount);

    if total_amount < invoice.amount.unwrap_or_default() {
        return false;
    }

    let mut total_tlc_amount = first_tlc.amount;
    for tlc in it {
        total_tlc_amount += tlc.amount;
    }

    debug!(
        "checking total_tlc_amount: {}, total_amount: {}",
        total_tlc_amount, total_amount
    );
    total_tlc_amount >= total_amount
}
