use fiber_types::{CchInvoice, CchOrder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentHandlerType {
    Fiber,
    Lightning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceHandlerType {
    Fiber,
    Lightning,
}

pub fn dispatch_invoice_handler(order: &CchOrder) -> InvoiceHandlerType {
    match order.incoming_invoice {
        CchInvoice::Fiber(_) => InvoiceHandlerType::Fiber,
        CchInvoice::Lightning(_) => InvoiceHandlerType::Lightning,
    }
}
pub fn dispatch_payment_handler(order: &CchOrder) -> PaymentHandlerType {
    // Payment use the inverse handler of the invoice.
    match order.incoming_invoice {
        CchInvoice::Fiber(_) => PaymentHandlerType::Lightning,
        CchInvoice::Lightning(_) => PaymentHandlerType::Fiber,
    }
}
