use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt::Display;

pub const RPC_LOG_PARAMS_ENV: &str = "FIBER_RPC_LOG_PARAMS";

/// ignore rpc-doc-gen
///
/// Extension trait on `Result` for concise RPC error conversion.
///
/// Replaces verbose `.map_err(|e| ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, e, None))`
/// chains with a single `.rpc_err()?` call.
pub trait RpcResultExt<T> {
    /// Convert the error into an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE`,
    /// without attaching request parameters to the error payload.
    fn rpc_err(self) -> Result<T, ErrorObjectOwned>;
}

impl<T, E: Display> RpcResultExt<T> for Result<T, E> {
    fn rpc_err(self) -> Result<T, ErrorObjectOwned> {
        self.map_err(|e| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Option::<()>::None,
            )
        })
    }
}

/// Construct an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE` and no data payload.
pub fn rpc_error(msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, msg, Option::<()>::None)
}

#[doc(hidden)]
pub fn should_log_rpc_params() -> bool {
    cfg!(debug_assertions)
        && std::env::var(RPC_LOG_PARAMS_ENV)
            .map(|value| {
                matches!(
                    value.as_str(),
                    "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
                )
            })
            .unwrap_or(false)
}

#[doc(hidden)]
pub fn redacted_rpc_params<T: Serialize>(params: &T) -> Value {
    let mut value =
        serde_json::to_value(params).unwrap_or_else(|_| Value::String("<params>".into()));
    redact_sensitive_fields(&mut value);
    value
}

fn redact_sensitive_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if is_sensitive_rpc_param_key(key) {
                    *value = Value::String("<redacted>".into());
                } else {
                    redact_sensitive_fields(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_sensitive_fields(value);
            }
        }
        _ => {}
    }
}

fn is_sensitive_rpc_param_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("private")
        || key.contains("privkey")
        || key.contains("preimage")
        || key.contains("settlement_key")
        || key == "local_key"
        || key.ends_with("_secret")
}

#[macro_export]
macro_rules! log_and_error {
    ($err:expr) => {{
        let err = $err;
        tracing::error!(error = ?err, "rpc request failed");
        Err($crate::rpc::utils::rpc_error(err))
    }};
    ($params:expr, $err:expr) => {{
        let err = $err;
        if $crate::rpc::utils::should_log_rpc_params() {
            tracing::error!(
                error = ?err,
                params = ?$crate::rpc::utils::redacted_rpc_params(&$params),
                "rpc request failed"
            );
        } else {
            tracing::error!(error = ?err, "rpc request failed");
        }
        Err($crate::rpc::utils::rpc_error(err))
    }};
}

#[macro_export]
macro_rules! handle_actor_call {
    ($actor:expr, $message:expr, $params:expr) => {
        match call!($actor, $message) {
            Ok(result) => match result {
                Ok(res) => Ok(res),
                Err(e) => log_and_error!($params, e.to_string()),
            },
            Err(e) => log_and_error!($params, e.to_string()),
        }
    };
    ($actor:expr, $message:expr) => {
        match call!($actor, $message) {
            Ok(result) => match result {
                Ok(res) => Ok(res),
                Err(e) => log_and_error!(e.to_string()),
            },
            Err(e) => log_and_error!(e.to_string()),
        }
    };
}
