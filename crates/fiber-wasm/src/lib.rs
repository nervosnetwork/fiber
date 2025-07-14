use std::{
    str::FromStr,
    sync::{Arc, atomic::AtomicU8},
};

use api::{FIBER_WASM, WrappedFiberWasm};
use ckb_chain_spec::ChainSpec;
use ckb_resource::Resource;
use fnn::{
    Config, NetworkServiceEvent,
    actors::RootActor,
    ckb::{
        CkbChainActor,
        contracts::{TypeIDResolver, try_init_contracts_context},
    },
    fiber::{KeyPair, channel::ChannelSubscribers, graph::NetworkGraph, network::init_chain_hash},
    rpc::{
        channel::ChannelRpcServerImpl,
        graph::GraphRpcServerImpl,
        info::InfoRpcServerImpl,
        invoice::InvoiceRpcServerImpl,
        payment::PaymentRpcServerImpl,
        peer::PeerRpcServerImpl,
        watchtower::{
            CreatePreimageParams, CreateWatchChannelParams, RemovePreimageParams,
            RemoveWatchChannelParams, UpdateLocalSettlementParams, UpdateRevocationParams,
            WatchtowerRpcClient,
        },
    },
    start_network,
    store::Store,
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
};
use jsonrpsee::wasm_client::WasmClientBuilder;
use ractor::Actor;
use secp256k1::{Secp256k1, SecretKey};
use std::fmt::Debug;
use tokio::{
    select,
    sync::{RwLock, mpsc},
};
use tracing::{debug, info, trace};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

pub mod api;

pub struct ExitMessage(String);
impl Debug for ExitMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Exit because {}", self.0)
    }
}

impl ExitMessage {
    pub fn err(message: String) -> Result<(), ExitMessage> {
        Err(ExitMessage(message))
    }
}
impl From<ExitMessage> for JsValue {
    fn from(val: ExitMessage) -> Self {
        JsValue::from_str(&val.0)
    }
}

const FIBER_STATE_BEFORE_STARTING: u8 = 0;
const FIBER_STATE_STARTED: u8 = 1;
const FIBER_STATE_PANICKED: u8 = 2;

static FIBER_STATE: AtomicU8 = AtomicU8::new(FIBER_STATE_BEFORE_STARTING);

pub(crate) fn check_state() -> Result<(), JsValue> {
    match FIBER_STATE.load(std::sync::atomic::Ordering::SeqCst) {
        FIBER_STATE_BEFORE_STARTING => ExitMessage::err("Fiber not started!".to_string()),
        FIBER_STATE_STARTED => Ok(()),
        FIBER_STATE_PANICKED => {
            ExitMessage::err("Fiber panicked, please refresh page".to_string())
        }
        s => ExitMessage::err(format!("Invalid FIBER_STATE: {}", s)),
    }
    .map_err(|e| e.into())
}

#[wasm_bindgen]
pub async fn fiber(
    config: &str,
    log_level: &str,
    spec: Option<String>,
    fiber_key_pair: Option<Vec<u8>>,
    ckb_secret_key: Option<Vec<u8>>,
) -> Result<(), ExitMessage> {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        FIBER_STATE.store(FIBER_STATE_PANICKED, std::sync::atomic::Ordering::SeqCst);
    }));
    wasm_logger::init(wasm_logger::Config::new(
        tracing::log::Level::from_str(log_level).expect("Bad log level"),
    ));

    info!(
        "Starting node with git version {} ({})",
        fnn::get_git_version(),
        fnn::get_git_commit_info()
    );

    let mut config = Config::parse_from_str(config);
    let fiber_key_pair = match fiber_key_pair.map(|value| KeyPair::try_from(&value[..]).unwrap()) {
        Some(v) => v,
        None => {
            tracing::warn!("Fiber KeyPair not provided, generating a random one..");
            KeyPair::generate_random_key()
        }
    };
    if let Some(ref mut value) = config.fiber {
        value.wasm_key_pair = Some(fiber_key_pair)
    }
    let ckb_secret_key =
        match ckb_secret_key.map(|value| SecretKey::from_slice(&value[..]).unwrap()) {
            Some(v) => v,
            None => {
                tracing::warn!("Ckb SecretKey not provided, generating a random one..");
                let mut rng = secp256k1::rand::thread_rng();
                Secp256k1::new().generate_keypair(&mut rng).0
            }
        };
    if let Some(ref mut value) = config.ckb {
        value.wasm_secret_key = Some(ckb_secret_key);
    }
    let store_path = config
        .fiber
        .as_ref()
        .ok_or_else(|| ExitMessage("fiber config is required but absent".to_string()))?
        .store_path();

    let store = Store::new(store_path).map_err(|err| ExitMessage(err.to_string()))?;
    debug!("Store initialized");
    let tracker = new_tokio_task_tracker();
    let token = new_tokio_cancellation_token();
    let root_actor = RootActor::start(tracker, token).await;
    let subscribers = ChannelSubscribers::default();

    #[allow(unused_variables)]
    let (network_actor, ckb_chain_actor, network_graph) = match config.fiber.clone() {
        Some(fiber_config) => {
            // TODO: this is not a super user friendly error message which has actionable information
            // for the user to fix the error and start the node.
            let ckb_config = config.ckb.clone().ok_or_else(|| {
                ExitMessage(
                    "service fiber requires service ckb which is not enabled in the config file"
                        .to_string(),
                )
            })?;
            let node_public_key = fiber_config.public_key();

            let chain = fiber_config.chain.as_str();
            let chain_spec = ChainSpec::load_from(&match chain {
                "mainnet" => Resource::bundled("specs/mainnet.toml".to_string()),
                "testnet" => Resource::bundled("specs/testnet.toml".to_string()),
                path => Resource::raw(
                    spec.expect("spec must be provided if chain is not mainnet nor testnet"),
                ),
            })
            .map_err(|err| ExitMessage(format!("failed to load chain spec: {}", err)))?;
            let genesis_block = chain_spec.build_genesis().map_err(|err| {
                ExitMessage(format!("failed to build ckb genesis block: {}", err))
            })?;

            init_chain_hash(genesis_block.hash().into());
            let type_id_resolver = TypeIDResolver::new(ckb_config.rpc_url.clone());
            try_init_contracts_context(
                genesis_block,
                fiber_config.scripts.clone(),
                ckb_config.udt_whitelist.clone().unwrap_or_default(),
                Some(type_id_resolver),
            )
            .await
            .map_err(|err| ExitMessage(format!("failed to init contracts context: {}", err)))?;

            let ckb_chain_actor = Actor::spawn_linked(
                Some("ckb".to_string()),
                CkbChainActor {},
                ckb_config.clone(),
                root_actor.get_cell(),
            )
            .await
            .map_err(|err| ExitMessage(format!("failed to start ckb actor: {}", err)))?
            .0;

            const CHANNEL_SIZE: usize = 4000;
            let (event_sender, mut event_receiver) = mpsc::channel(CHANNEL_SIZE);

            let network_graph = Arc::new(RwLock::new(NetworkGraph::new(
                store.clone(),
                node_public_key.clone().into(),
                fiber_config.announce_private_addr(),
            )));

            // we use the default funding lock script as the shutdown script for the network actor
            let default_shutdown_script = ckb_config
                .get_default_funding_lock_script()
                .expect("get default funding lock script should be ok");

            info!("Starting fiber");
            let network_actor = start_network(
                fiber_config.clone(),
                ckb_chain_actor.clone(),
                event_sender,
                new_tokio_task_tracker(),
                root_actor.get_cell(),
                store.clone(),
                subscribers.clone(),
                network_graph.clone(),
                default_shutdown_script,
            )
            .await;

            if fiber_config.standalone_watchtower_rpc_url.is_none()
                && fiber_config.disable_built_in_watchtower.unwrap_or_default()
            {
                return ExitMessage::err(
                    "fiber config requires standalone watchtower rpc url or built-in watchtower to be enabled"
                        .to_string(),
                );
            }

            let watchtower_client = if let Some(url) = fiber_config.standalone_watchtower_rpc_url {
                let watchtower_client =
                    WasmClientBuilder::default()
                        .build(url)
                        .await
                        .map_err(|err| {
                            ExitMessage(format!("failed to create watchtower rpc client: {}", err))
                        })?;
                Some(watchtower_client)
            } else {
                None
            };

            ractor::concurrency::spawn(async move {
                let token = new_tokio_cancellation_token();
                loop {
                    select! {
                        event = event_receiver.recv() => {
                            match event {
                                None => {
                                    trace!("Event receiver completed, stopping event processing service");
                                    break;
                                }
                                Some(event) => {
                                    if let Some(watchtower_client) = watchtower_client.as_ref() {
                                        forward_event_to_client(event.clone(), watchtower_client).await;
                                    }
                                }
                            }
                        }
                        _ = token.cancelled() => {
                            debug!("Cancellation received, stopping event processing service");
                            break;
                        }
                    }
                }
                debug!("Event processing service exited");
            });

            (
                Some(network_actor),
                Some(ckb_chain_actor),
                Some(network_graph),
            )
        }
        None => (None, None, None),
    };
    debug!("Network actor is_none = {}", network_actor.is_none());
    let network_actor = network_actor.unwrap();
    let network_graph = network_graph.unwrap();
    if FIBER_WASM
        .set(WrappedFiberWasm {
            channel: ChannelRpcServerImpl::new(network_actor.clone(), store.clone()),
            graph: GraphRpcServerImpl::new(network_graph.clone(), store.clone()),
            info: InfoRpcServerImpl::new(network_actor.clone(), config.ckb.unwrap_or_default()),
            invoice: InvoiceRpcServerImpl::new(store.clone(), config.fiber),
            payment: PaymentRpcServerImpl::new(network_actor.clone(), store.clone()),
            peer: PeerRpcServerImpl::new(network_actor.clone()),
        })
        .is_err()
    {
        panic!("FIBER_WASM is already set!");
    } else {
        debug!("WrappedFiberWasm set");
    }
    FIBER_STATE.store(FIBER_STATE_STARTED, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

const ASSUME_WATCHTOWER_CLIENT_CALL_OK: &str = "watchtower client call should be ok";

async fn forward_event_to_client<T: WatchtowerRpcClient + Sync>(
    event: NetworkServiceEvent,
    watchtower_client: &T,
) {
    match event {
        NetworkServiceEvent::RemoteTxComplete(
            _peer_id,
            channel_id,
            funding_tx_lock,
            remote_settlement_data,
        ) => {
            watchtower_client
                .create_watch_channel(CreateWatchChannelParams {
                    channel_id,
                    funding_tx_lock: funding_tx_lock.into(),
                    remote_settlement_data,
                })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        NetworkServiceEvent::ChannelClosed(_, channel_id, _)
        | NetworkServiceEvent::ChannelAbandon(channel_id) => {
            watchtower_client
                .remove_watch_channel(RemoveWatchChannelParams { channel_id })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        NetworkServiceEvent::RevokeAndAckReceived(
            _peer_id,
            channel_id,
            revocation_data,
            settlement_data,
        ) => {
            watchtower_client
                .update_revocation(UpdateRevocationParams {
                    channel_id,
                    revocation_data,
                    settlement_data,
                })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        NetworkServiceEvent::RemoteCommitmentSigned(
            _peer_id,
            channel_id,
            _commitment_tx,
            settlement_data,
        ) => {
            watchtower_client
                .update_local_settlement(UpdateLocalSettlementParams {
                    channel_id,
                    settlement_data,
                })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        NetworkServiceEvent::PreimageCreated(payment_hash, preimage) => {
            watchtower_client
                .create_preimage(CreatePreimageParams {
                    payment_hash,
                    preimage,
                })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        NetworkServiceEvent::PreimageRemoved(payment_hash) => {
            watchtower_client
                .remove_preimage(RemovePreimageParams { payment_hash })
                .await
                .expect(ASSUME_WATCHTOWER_CLIENT_CALL_OK);
        }
        _ => {
            // ignore other non-watchtower related events
        }
    }
}
