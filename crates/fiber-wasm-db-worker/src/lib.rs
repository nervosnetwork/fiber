use std::{cell::RefCell, str::FromStr};

use db::{STORE_NAME, handle_db_command, open_database};
use fiber_wasm_db_common::{
    InputCommand, KV, OutputCommand, load_command, read_command_payload, store_command,
    write_command_with_payload,
};
use idb::Database;
use log::info;
use util::{wait_for_command, wait_for_command_sync};
use wasm_bindgen::{JsCast, prelude::wasm_bindgen};
use web_sys::{
    js_sys::{Int32Array, SharedArrayBuffer, Uint8Array},
    wasm_bindgen::JsValue,
};

mod db;
mod util;
thread_local! {
    static INPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
    static OUTPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
}
/// Set `SharedArrayBuffer` used for communicating with light client worker. This must be called before executing `main_loop`
/// input - The buffer used for sending data from light client worker to db worker
/// output - The buffer used for sending data from db worker to light client worker
#[wasm_bindgen]
pub fn set_shared_array(input: JsValue, output: JsValue) {
    console_error_panic_hook::set_once();
    INPUT_BUFFER.with(|v| {
        *v.borrow_mut() = Some(
            input
                .dyn_into()
                .expect("input buffer must be a SharedArrayBuffer"),
        );
    });
    OUTPUT_BUFFER.with(|v| {
        *v.borrow_mut() = Some(
            output
                .dyn_into()
                .expect("output buffer must be a SharedArrayBuffer"),
        );
    });
}
#[wasm_bindgen]
/// Enter the main loop of db worker. Once entered, db worker will read commands from input buffer (previously set by set_shared_array), handle it, and write response to output buffer.
/// log_level - Level of logs, such as `debug`, `info`.
pub async fn main_loop(log_level: &str) {
    wasm_logger::init(wasm_logger::Config::new(
        log::Level::from_str(log_level).expect("Invalid log level"),
    ));

    let (input_i32_arr, input_u8_arr) = INPUT_BUFFER.with(|x| {
        let binding = x.borrow();
        let buf = binding.as_ref().unwrap();
        (Int32Array::new(buf), Uint8Array::new(buf))
    });
    let (output_i32_arr, output_u8_arr) = OUTPUT_BUFFER.with(|x| {
        let binding = x.borrow();
        let buf = binding.as_ref().unwrap();
        (Int32Array::new(buf), Uint8Array::new(buf))
    });

    let mut db: Option<Database> = None;
    log::debug!("Entered main loop of db worker");
    loop {
        let cmd = wait_for_command(&input_i32_arr, InputCommand::Waiting)
            .await
            .expect("Unable to wait for command");
        // Clean it to avoid infinite loop
        store_command(&input_i32_arr, InputCommand::Waiting as i32)
            .expect("Unable to reset input command");
        match cmd {
            InputCommand::OpenDatabase => {
                let database_name =
                    read_command_payload::<String>(&input_i32_arr, &input_u8_arr).unwrap();
                match open_database(&database_name).await {
                    Ok(o) => {
                        db = Some(o);
                        write_command_with_payload(
                            OutputCommand::OpenDatabaseResponse as i32,
                            true,
                            &output_i32_arr,
                            &output_u8_arr,
                        )
                        .unwrap();
                    }
                    Err(err) => {
                        log::error!("Failed to open database: {:?}", err);
                        write_command_with_payload(
                            OutputCommand::Error as i32,
                            format!("{:?}", err),
                            &output_i32_arr,
                            &output_u8_arr,
                        )
                        .unwrap();
                    }
                }
            }
            InputCommand::DbRequest => {
                let db_cmd = read_command_payload(&input_i32_arr, &input_u8_arr).unwrap();
                let db = db.as_ref().expect("Database not opened yet");
                let result = handle_db_command(db, STORE_NAME, db_cmd, |key| {
                    store_command(&input_i32_arr, InputCommand::Waiting as i32)
                        .expect("Unable to reset input command");
                    write_command_with_payload(
                        OutputCommand::RequestTakeWhile as i32,
                        key.to_vec(),
                        &output_i32_arr,
                        &output_u8_arr,
                    )
                    .unwrap();
                    // Sync wait here, so transaction of IndexedDB won't be committed (it will be committed once control flow was returned from sync call stack)
                    wait_for_command_sync(&input_i32_arr, InputCommand::Waiting).unwrap();

                    let raw_cmd =
                        load_command(&input_i32_arr).expect("Unable to load input command");
                    let cmd =
                        InputCommand::try_from(raw_cmd).expect("Invalid input command from client");
                    assert!(
                        matches!(cmd, InputCommand::ResponseTakeWhile),
                        "Expected ResponseTakeWhile, got {:?}",
                        raw_cmd
                    );

                    let result =
                        read_command_payload::<bool>(&input_i32_arr, &input_u8_arr).unwrap();
                    store_command(&input_i32_arr, InputCommand::Waiting as i32)
                        .expect("Unable to reset input command");
                    result
                })
                .await;
                match result {
                    Ok(o) => write_command_with_payload(
                        OutputCommand::DbResponse as i32,
                        &o,
                        &output_i32_arr,
                        &output_u8_arr,
                    )
                    .unwrap(),
                    Err(e) => write_command_with_payload(
                        OutputCommand::Error as i32,
                        format!("{:?}", e),
                        &output_i32_arr,
                        &output_u8_arr,
                    )
                    .unwrap(),
                };
            }
            InputCommand::Shutdown => break,
            InputCommand::Waiting => {}
            InputCommand::ResponseTakeWhile => {
                log::error!("Ignoring stale ResponseTakeWhile received in db worker main loop");
            }
        }
    }
    info!("Db worker main loop exited");
}
