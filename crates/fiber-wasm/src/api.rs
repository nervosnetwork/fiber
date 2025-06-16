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
use jsonrpsee::{core::Serialize, types::ErrorObjectOwned};
use serde::de::DeserializeOwned;
use wasm_bindgen::JsValue;

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
    FIBER_WASM
        .get()
        .ok_or_else(|| JsValue::from_str("Fiber wasm not started yet"))
}

fn param<T: DeserializeOwned>(input: JsValue) -> Result<T, JsValue> {
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

pub mod graph {
    use wasm_bindgen::JsValue;

    use super::{error, fiber_wasm, param, result};

    pub async fn graph_nodes(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .graph
            .graph_nodes(param(params)?)
            .await
            .map_err(error)
            .map(result)
    }

    pub async fn graph_channels(params: JsValue) -> Result<JsValue, JsValue> {
        fiber_wasm()?
            .graph
            .graph_channels(param(params)?)
            .await
            .map_err(error)
            .map(result)
    }
}

pub mod info {}
pub mod invoice {}
pub mod payment {}
pub mod peer {}
