#[macro_export]
macro_rules! log_and_error {
    ($params:expr, $err:expr) => {{
        tracing::error!("channel request params {:?} => error: {:?}", $params, $err);
        Err(jsonrpsee::types::ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            $err,
            Some($params),
        ))
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

#[macro_export]
macro_rules! handle_actor_cast {
    ($actor:expr, $message:expr, $params:expr) => {
        match $actor.cast($message) {
            Ok(_) => Ok(()),
            Err(err) => log_and_error!($params, format!("{}", err)),
        }
    };
}
