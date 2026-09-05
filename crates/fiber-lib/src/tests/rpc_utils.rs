#[cfg(test)]
mod tests {
    use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
    use serde_json::json;

    use crate::rpc::utils::{redacted_rpc_params, rpc_error, RpcResultExt};

    #[test]
    fn rpc_error_omits_request_data() {
        let err = rpc_error("invalid private key");

        assert_eq!(err.code(), CALL_EXECUTION_FAILED_CODE);
        assert_eq!(err.message(), "invalid private key");
        assert!(err.data().is_none());
    }

    #[test]
    fn rpc_err_omits_request_data() {
        let err = Result::<(), &str>::Err("actor failed")
            .rpc_err()
            .unwrap_err();

        assert_eq!(err.code(), CALL_EXECUTION_FAILED_CODE);
        assert_eq!(err.message(), "actor failed");
        assert!(err.data().is_none());
    }

    #[test]
    fn rpc_param_debug_redacts_sensitive_fields() {
        let private_key = "1111111111111111111111111111111111111111111111111111111111111111";
        let payment_preimage = "2222222222222222222222222222222222222222222222222222222222222222";
        let params = json!({
            "private_key": private_key,
            "nested": {
                "payment_preimage": payment_preimage,
                "public_field": "kept",
            },
            "items": [{
                "local_settlement_key": "3333333333333333333333333333333333333333333333333333333333333333",
            }],
        });

        let redacted = redacted_rpc_params(&params);
        let rendered = redacted.to_string();

        assert_eq!(redacted["private_key"], "<redacted>");
        assert_eq!(redacted["nested"]["payment_preimage"], "<redacted>");
        assert_eq!(redacted["nested"]["public_field"], "kept");
        assert_eq!(redacted["items"][0]["local_settlement_key"], "<redacted>");
        assert!(!rendered.contains(private_key));
        assert!(!rendered.contains(payment_preimage));
    }

    #[test]
    fn rpc_param_debug_redacts_nested_client_invoice() {
        let params = json!({
            "quote": {
                "client_invoice": "nested-secret",
                "asset": {
                    "asset_id": "ckb",
                },
            },
        });

        let redacted = redacted_rpc_params(&params);

        assert_eq!(redacted["quote"]["client_invoice"], "<redacted>");
        assert_eq!(redacted["quote"]["asset"]["asset_id"], "ckb");
    }

    #[test]
    fn rpc_param_debug_redacts_direct_invoice() {
        let params = json!({
            "invoice": "direct-secret",
            "amount": "0x64",
        });

        let redacted = redacted_rpc_params(&params);

        assert_eq!(redacted["invoice"], "<redacted>");
        assert_eq!(redacted["amount"], "0x64");
    }
}
