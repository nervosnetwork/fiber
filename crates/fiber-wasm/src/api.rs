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
