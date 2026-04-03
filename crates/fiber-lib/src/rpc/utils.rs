use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use serde::Serialize;
use std::fmt::Display;

/// ignore rpc-doc-gen
///
/// Extension trait on `Result` for concise RPC error conversion.
///
/// Replaces verbose `.map_err(|e| ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, e, Some(&params)))`
/// chains with a single `.rpc_err(&params)?` call.
pub trait RpcResultExt<T> {
    /// Convert the error into an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE`,
    /// attaching `data` as the `Some(data)` payload.
    fn rpc_err<D: Serialize>(self, data: &D) -> Result<T, ErrorObjectOwned>;

    /// Convert the error into an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE`,
    /// without any data payload (`Option::<()>::None`).
    fn rpc_err_no_data(self) -> Result<T, ErrorObjectOwned>;
}

impl<T, E: Display> RpcResultExt<T> for Result<T, E> {
    fn rpc_err<D: Serialize>(self, data: &D) -> Result<T, ErrorObjectOwned> {
        self.map_err(|e| {
            ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, e.to_string(), Some(data))
        })
    }

    fn rpc_err_no_data(self) -> Result<T, ErrorObjectOwned> {
        self.map_err(|e| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Option::<()>::None,
            )
        })
    }
}

/// Construct an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE` and a data payload.
pub fn rpc_error<D: Serialize>(msg: impl Into<String>, data: D) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, msg, Some(data))
}

/// Construct an `ErrorObjectOwned` with `CALL_EXECUTION_FAILED_CODE` and no data payload.
pub fn rpc_error_no_data(msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, msg, Option::<()>::None)
}

#[macro_export]
macro_rules! log_and_error {
    ($params:expr, $err:expr) => {{
        tracing::error!("channel request params {:?} => error: {:?}", $params, $err);
        Err($crate::rpc::utils::rpc_error($err, $params))
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
                Err(e) => {
                    error!("Error: {:?}", e);
                    Err(e)
                }
            },
            Err(e) => {
                error!("Error: {:?}", e);
                Err(e)
            }
        }
    };
}
