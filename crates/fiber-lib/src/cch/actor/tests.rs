use super::*;

use super::{is_lnd_invoice_not_found, DeferredInvoiceStatus, LND_NO_INVOICES_CREATED_ERROR};
use crate::invoice::CkbInvoiceStatus;

#[test]
fn test_deferred_invoice_status_merge_does_not_regress() {
    let mut deferred = DeferredInvoiceStatus {
        status: CkbInvoiceStatus::Received,
        failure_reason: None,
    };

    deferred.merge(DeferredInvoiceStatus {
        status: CkbInvoiceStatus::Open,
        failure_reason: None,
    });

    assert_eq!(deferred.status, CkbInvoiceStatus::Received);
}

#[test]
fn test_lnd_invoice_not_found_status_classification() {
    assert!(is_lnd_invoice_not_found(&tonic::Status::not_found(
        "unable to locate invoice",
    )));
    assert!(is_lnd_invoice_not_found(&tonic::Status::unknown(
        LND_NO_INVOICES_CREATED_ERROR,
    )));
    assert!(!is_lnd_invoice_not_found(&tonic::Status::unknown(
        "database unavailable",
    )));
}

/// Test-only CCH messages accepted through `CchMessage::Test`.
pub enum TestMessage {
    InsertOrder(CchOrder, RpcReplyPort<Result<(), CchError>>),
    GetStartupRecoveryStatus(RpcReplyPort<bool>),
}

pub(crate) fn handle_test_message<S: CchOrderStore>(state: &mut CchState<S>, message: TestMessage) {
    match message {
        TestMessage::InsertOrder(order, port) => {
            let result = state.store.insert_cch_order(order).map_err(Into::into);
            if !port.is_closed() {
                let _ = port.send(result);
            }
        }
        TestMessage::GetStartupRecoveryStatus(port) => {
            let _ = port.send(state.startup_recovery_in_progress);
        }
    }
}
