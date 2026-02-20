use anyhow::Context;
use anyhow::anyhow;
use fiber_wasm_db_common::InputCommand;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys::Promise;
use web_sys::js_sys::{Atomics, Int32Array};
/// Wrapper of Atomics.wait
pub(crate) fn wait_for_command_sync(
    state_arr: &Int32Array,
    holding_command: InputCommand,
) -> anyhow::Result<InputCommand> {
    Atomics::wait(state_arr, 0, holding_command as i32)
        .map_err(|e| anyhow!("Failed to call wait async: {:?}", e))?;

    InputCommand::try_from(state_arr.get_index(0)).with_context(|| anyhow!("Bad command"))
}

#[derive(Deserialize)]
struct WaitAsyncResponse {
    #[serde(with = "serde_wasm_bindgen::preserve")]
    value: JsValue,
    #[serde(rename = "async")]
    async_: bool,
}
/// Wrapper of Atomics.waitAsync
pub(crate) async fn wait_for_command(
    state_arr: &Int32Array,
    holding_command: InputCommand,
) -> anyhow::Result<InputCommand> {
    let obj = Atomics::wait_async(state_arr, 0, holding_command as i32)
        .map_err(|e| anyhow!("Failed to call wait async: {:?}", e))?;
    let resp = serde_wasm_bindgen::from_value::<WaitAsyncResponse>((&obj).into())
        .map_err(|e| anyhow!("Failed to deserialize result of wait async: {:?}", e))?;

    if resp.async_ {
        JsFuture::from(
            resp.value
                .dyn_into::<Promise>()
                .expect("Value must be casted into promise!"),
        )
        .await
        .expect("value must not fail!");
    } else {
        let value = resp
            .value
            .as_string()
            .expect("Value must be casted to string");
        if value != "not-equal" {
            panic!("Value must be not-equal if should_wait_async is false");
        }
    }

    InputCommand::try_from(state_arr.get_index(0)).with_context(|| anyhow!("Bad command"))
}
