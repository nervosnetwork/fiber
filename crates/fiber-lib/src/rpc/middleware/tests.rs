use super::*;

#[test]
fn test_auth_reject_error_reports_run_limit_reason() {
    let cases = [
        (
            RunLimit::Timeout,
            "Unauthorized: Biscuit authorization timed out",
        ),
        (
            RunLimit::TooManyFacts,
            "Unauthorized: Biscuit authorization generated too many facts",
        ),
        (
            RunLimit::TooManyIterations,
            "Unauthorized: Biscuit authorization exceeded the iteration limit",
        ),
        (
            RunLimit::UnexpectedQueryResult(1, 2),
            "Unauthorized: Biscuit authorization returned unexpected query results",
        ),
    ];

    for (limit, message) in cases {
        let error = Error::new(BiscuitError::RunLimit(limit));
        let rpc_error = auth_reject_error("test_method", &error);

        assert_eq!(rpc_error.code(), -32999);
        assert_eq!(rpc_error.message(), message);
    }
}

#[test]
fn test_auth_reject_error_keeps_generic_unauthorized_message() {
    let error = anyhow!("missing token");
    let rpc_error = auth_reject_error("test_method", &error);

    assert_eq!(rpc_error.code(), -32999);
    assert_eq!(rpc_error.message(), "Unauthorized");
}
