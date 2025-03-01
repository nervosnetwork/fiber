#[macro_export]
macro_rules! log_and_error {
    ($params:expr_2021, $err:expr_2021) => {{
        tracing::error!("channel request params {:?} => error: {:?}", $params, $err);
        Err(ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            $err,
            Some($params),
        ))
    }};
}

#[macro_export]
macro_rules! handle_actor_call {
    ($actor:expr_2021, $message:expr_2021, $params:expr_2021) => {
        match call!($actor, $message) {
            Ok(result) => match result {
                Ok(res) => Ok(res),
                Err(e) => log_and_error!($params, e.to_string()),
            },
            Err(e) => log_and_error!($params, e.to_string()),
        }
    };
    ($actor:expr_2021, $message:expr_2021) => {
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
    ($actor:expr_2021, $message:expr_2021, $params:expr_2021) => {
        match $actor.cast($message) {
            Ok(_) => Ok(()),
            Err(err) => log_and_error!($params, format!("{}", err)),
        }
    };
}
