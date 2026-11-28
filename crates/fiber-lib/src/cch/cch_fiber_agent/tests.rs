use super::*;

#[test]
fn http_invoice_expiry_reserves_the_request_timeout_before_deadline() {
    let request_started_millis = 10_250;
    let deadline_seconds = 100;
    let expiry_seconds =
        bounded_http_invoice_expiry_seconds(deadline_seconds, 10, request_started_millis)
            .expect("enough time remains");
    let latest_remote_timestamp_millis =
        u128::from(request_started_millis) + FIBER_HTTP_REQUEST_TIMEOUT.as_millis() - 1;

    assert!(
        latest_remote_timestamp_millis + u128::from(expiry_seconds) * 1_000
            <= u128::from(deadline_seconds) * 1_000
    );
}

#[test]
fn http_invoice_expiry_rejects_an_insufficient_request_window() {
    let now_millis = 10_250;
    let now_seconds_ceiling = 11;
    let min_expiry_seconds = 10;
    let deadline_seconds =
        now_seconds_ceiling + FIBER_HTTP_REQUEST_TIMEOUT.as_secs() + min_expiry_seconds - 1;

    assert!(
        bounded_http_invoice_expiry_seconds(deadline_seconds, min_expiry_seconds, now_millis,)
            .is_err()
    );
}

#[test]
fn cancel_invoice_error_classification_uses_error_type() {
    assert!(matches!(
        CchFiberCancelInvoiceError::from(CancelInvoiceError::InvoiceNotFound),
        CchFiberCancelInvoiceError::Permanent(_)
    ));
    assert!(matches!(
        CchFiberCancelInvoiceError::from(CancelInvoiceError::InvoiceAlreadyPaid),
        CchFiberCancelInvoiceError::Permanent(_)
    ));
    assert!(matches!(
        CchFiberCancelInvoiceError::from(CancelInvoiceError::PaymentPreimageAlreadyExists),
        CchFiberCancelInvoiceError::Permanent(_)
    ));
    assert!(matches!(
        CchFiberCancelInvoiceError::from(CancelInvoiceError::InternalError(
            "store failure".to_string()
        )),
        CchFiberCancelInvoiceError::Transient(_)
    ));
}
