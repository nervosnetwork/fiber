use std::sync::OnceLock;

use fnn::{
    fiber::{
        channel::ChannelActorStateStore, gossip::GossipMessageStore, graph::NetworkGraphStateStore,
    },
    invoice::InvoiceStore,
    rpc::{
        channel::ChannelRpcServerImpl, graph::GraphRpcServerImpl, info::InfoRpcServerImpl,
        invoice::InvoiceRpcServerImpl, payment::PaymentRpcServerImpl, peer::PeerRpcServerImpl,
    },
    store::Store,
};
use js_sys::{Array, Object, Reflect};
use jsonrpsee::{core::Serialize, types::ErrorObjectOwned};
use serde::de::DeserializeOwned;
use wasm_bindgen::JsValue;

use crate::check_state;

pub(crate) struct FiberWasm<
    ChannelStoreType: ChannelActorStateStore + Send + Sync + 'static,
    GraphStoreType: NetworkGraphStateStore
        + ChannelActorStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
    InvoiceStoreType: InvoiceStore + Send + Sync + 'static,
    PaymentStoreType: ChannelActorStateStore + Send + Sync + 'static,
> {
    pub(crate) channel: ChannelRpcServerImpl<ChannelStoreType>,
    pub(crate) graph: GraphRpcServerImpl<GraphStoreType>,
    pub(crate) info: InfoRpcServerImpl,
    pub(crate) invoice: InvoiceRpcServerImpl<InvoiceStoreType>,
    pub(crate) payment: PaymentRpcServerImpl<PaymentStoreType>,
    pub(crate) peer: PeerRpcServerImpl,
}
pub(crate) type WrappedFiberWasm = FiberWasm<Store, Store, Store, Store>;

pub(crate) static FIBER_WASM: OnceLock<WrappedFiberWasm> = OnceLock::new();

fn fiber_wasm() -> Result<&'static WrappedFiberWasm, JsValue> {
    check_state()?;
    FIBER_WASM
        .get()
        .ok_or_else(|| JsValue::from_str("Fiber wasm not started yet"))
}

/// Recursively normalize CKB `dep_type` / `depType` field values in JS objects.
///
/// CKB JSON-RPC and some JS libraries (e.g. Lumos) use `"dep_group"` for the
/// dep_type value, but the CKB JSON-RPC types serialized by `ckb-jsonrpc-types`
/// via serde expect `"dep_group"` while some wallets/SDKs produce `"depGroup"`
/// (camelCase variant). This function walks the JS object tree and rewrites any
/// `"depGroup"` occurrences to `"dep_group"` so that serde deserialization
/// succeeds when converting JS values to Rust types.
///
/// TODO: Ideally this should be handled at the serde layer (e.g. with a custom
/// deserializer or `#[serde(alias)]`), removing the need for runtime patching.
fn normalize_dep_type(value: &JsValue) {
    if value.is_null() || value.is_undefined() {
        return;
    }

    if Array::is_array(value) {
        let arr = Array::from(value);
        for i in 0..arr.length() {
            normalize_dep_type(&arr.get(i));
        }
        return;
    }

    if !value.is_object() {
        return;
    }

    let obj = Object::from(value.clone());
    for key in ["dep_type", "depType"] {
        let key_js = JsValue::from_str(key);
        if Reflect::has(&obj, &key_js).unwrap_or(false)
            && let Ok(v) = Reflect::get(&obj, &key_js)
        {
            let v: JsValue = v;
            if v.is_string() && v.as_string().as_deref() == Some("depGroup") {
                let _ = Reflect::set(&obj, &key_js, &JsValue::from_str("dep_group"));
            }
        }
    }

    let keys = Object::keys(&obj);
    for i in 0..keys.length() {
        if let Some(key) = keys.get(i).as_string()
            && let Ok(child) = Reflect::get(&obj, &JsValue::from_str(&key))
        {
            normalize_dep_type(&child);
        }
    }
}

fn param<T: DeserializeOwned>(input: JsValue) -> Result<T, JsValue> {
    normalize_dep_type(&input);
    serde_wasm_bindgen::from_value(input).map_err(Into::into)
}

fn result<T: Serialize>(input: T) -> JsValue {
    serde_wasm_bindgen::to_value(&input).unwrap()
}

fn error(err: ErrorObjectOwned) -> JsValue {
    JsValue::from_str(&format!("{:?}", err))
}

pub mod channel {

    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, param, result};
    #[wasm_bindgen]
    pub async fn open_channel(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .channel
            .open_channel(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn accept_channel(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .channel
            .accept_channel(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn abandon_channel(params: JsValue) -> Result<(), JsValue> {
        fiber_wasm()?
            .channel
            .abandon_channel(param(params)?)
            .await
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn list_channels(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .channel
            .list_channels(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn shutdown_channel(params: JsValue) -> Result<(), JsValue> {
        fiber_wasm()?
            .channel
            .shutdown_channel(param(params)?)
            .await
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn update_channel(params: JsValue) -> Result<(), JsValue> {
        fiber_wasm()?
            .channel
            .update_channel(param(params)?)
            .await
            .map_err(error)
    }
}

/// LocalSign: open channel with external funding (user signs tx locally).
/// Accepts the same params as the native RPC, including optional
/// `funding_source_extra_cell_deps`.
/// Exported at crate root in lib.rs for fiber-js worker lookup.
pub async fn open_channel_with_external_funding(params: JsValue) -> Result<JsValue, JsValue> {
    fiber_wasm()?
        .channel
        .open_channel_with_external_funding(param(params)?)
        .await
        .map(result)
        .map_err(error)
}

/// LocalSign: submit signed funding tx after user signs locally.
/// Exported at crate root in lib.rs for fiber-js worker lookup.
pub async fn submit_signed_funding_tx(params: JsValue) -> Result<JsValue, JsValue> {
    fiber_wasm()?
        .channel
        .submit_signed_funding_tx(param(params)?)
        .await
        .map(result)
        .map_err(error)
}

pub mod graph {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, param, result};
    #[wasm_bindgen]
    pub async fn graph_nodes(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .graph
            .graph_nodes(param(params)?)
            .await
            .map_err(error)
            .map(result)
    }
    #[wasm_bindgen]
    pub async fn graph_channels(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .graph
            .graph_channels(param(params)?)
            .await
            .map_err(error)
            .map(result)
    }
}

pub mod info {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, result};
    #[wasm_bindgen]
    pub async fn node_info() -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .info
            .node_info()
            .await
            .map(result)
            .map_err(error)
    }
}
pub mod invoice {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, param, result};
    #[wasm_bindgen]
    pub async fn new_invoice(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .invoice
            .new_invoice(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn parse_invoice(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .invoice
            .parse_invoice(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn get_invoice(payment_hash: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .invoice
            .get_invoice(param(payment_hash)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn cancel_invoice(payment_hash: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .invoice
            .cancel_invoice(param(payment_hash)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn settle_invoice(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .invoice
            .settle_invoice(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
}
pub mod payment {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, param, result};
    #[wasm_bindgen]
    pub async fn send_payment(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .payment
            .send_payment(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn get_payment(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .payment
            .get_payment(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn build_router(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .payment
            .build_router(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn send_payment_with_router(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .payment
            .send_payment_with_router(param(params)?)
            .await
            .map(result)
            .map_err(error)
    }
}
pub mod peer {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::{error, fiber_wasm, param, result};
    #[wasm_bindgen]
    pub async fn connect_peer(params: JsValue) -> Result<(), JsValue> {
        fiber_wasm()?
            .peer
            .connect_peer(param(params)?)
            .await
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn disconnect_peer(params: JsValue) -> Result<(), JsValue> {
        fiber_wasm()?
            .peer
            .disconnect_peer(param(params)?)
            .await
            .map_err(error)
    }
    #[wasm_bindgen]
    pub async fn list_peers() -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .peer
            .list_peers()
            .await
            .map_err(error)
            .map(result)
    }
}
