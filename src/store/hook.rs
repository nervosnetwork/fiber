//! Store update hook. We can optionally implement a few hooks to run our own logic when the store is updated.
//! For example, we can push out a notification when an invoice is paid. This mod contains the traits definition
//! and a default implementation that does nothing. The mod subscription_impl contains an implementation that
//! sends out payment/invoice update events to subscribers.

use crate::{
    fiber::{graph::PaymentSessionStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

pub trait InvoiceUpdateHook: Send + Clone {
    fn on_invoice_updated(&self, invoice_hash: Hash256, status: CkbInvoiceStatus);
}

pub trait PaymentUpdateHook: Send + Clone {
    fn on_payment_updated(&self, payment_hash: Hash256, status: PaymentSessionStatus);
}

pub type NoopStoreUpdateHook = ();

impl InvoiceUpdateHook for NoopStoreUpdateHook {
    #[inline]
    fn on_invoice_updated(&self, _invoice_hash: Hash256, _status: CkbInvoiceStatus) {}
}

impl PaymentUpdateHook for NoopStoreUpdateHook {
    #[inline]
    fn on_payment_updated(&self, _payment_hash: Hash256, _status: PaymentSessionStatus) {}
}
