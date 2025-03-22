use ckb_chain_spec::ChainSpec;
use ckb_resource::Resource;
use core::default::Default;
use fnn::actors::RootActor;
use fnn::cch::CchMessage;
use fnn::ckb::{contracts::try_init_contracts_context, CkbChainActor};
use fnn::fiber::{channel::ChannelSubscribers, graph::NetworkGraph, network::init_chain_hash};
use fnn::store::Store;
use fnn::tasks::{
    cancel_tasks_and_wait_for_completion, new_tokio_cancellation_token, new_tokio_task_tracker,
};
use fnn::watchtower::{
    WatchtowerActor, WatchtowerMessage, DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS,
};
#[cfg(debug_assertions)]
use fnn::NetworkServiceEvent;
use fnn::{start_cch, start_network, start_rpc, Config};
use ractor::Actor;
#[cfg(debug_assertions)]
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, info_span, trace};
use tracing_subscriber::{field::MakeExt, fmt, fmt::format, EnvFilter};

pub struct ExitMessage(String);

#[tokio::main]
pub async fn main() -> Result<(), ExitMessage> {
    // ractor will set "id" for each actor:
    // https://github.com/slawlor/ractor/blob/67d657e4cdcb8884a9ccc9b758704cbb447ac163/ractor/src/actor/mod.rs#L701
    // here we map it with the node prefix
    let node_formatter = format::debug_fn(|writer, field, value| {
        let prefix = if field.name() == "id" {
            let r = fnn::get_node_prefix();
            if !r.is_empty() {
                format!(" on {}", r)
            } else {
                "".to_string()
            }
        } else {
            "".to_string()
        };
        write!(writer, "{}: {:?}{}", field, value, prefix)
    })
    .delimited(", ");
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .pretty()
        .fmt_fields(node_formatter)
        .try_init()
        .map_err(|err| ExitMessage(format!("failed to initialize logger: {}", err)))?;

    info!("Starting node with git version {}", fnn::get_git_version());

    let _span = info_span!("node", node = fnn::get_node_prefix()).entered();

    let config = Config::parse();

    let store_path = config
        .fiber
        .as_ref()
        .ok_or_else(|| ExitMessage("fiber config is required but absent".to_string()))?
        .store_path();

    let store = Store::new(store_path).map_err(|err| ExitMessage(err.to_string()))?;

    let tracker = new_tokio_task_tracker();
    let token = new_tokio_cancellation_token();
    let root_actor = RootActor::start(tracker, token).await;
    let subscribers = ChannelSubscribers::default();

    #[cfg(debug_assertions)]
    let rpc_dev_module_commitment_txs = config.rpc.as_ref().and_then(|rpc_config| {
        if rpc_config.is_module_enabled("dev") {
            Some(Arc::new(RwLock::new(HashMap::new())))
        } else {
            None
        }
    });

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
                path => Resource::file_system(Path::new(&config.base_dir).join(path)),
            })
            .map_err(|err| ExitMessage(format!("failed to load chain spec: {}", err)))?;
            let genesis_block = chain_spec.build_genesis().map_err(|err| {
                ExitMessage(format!("failed to build ckb genesis block: {}", err))
            })?;

            init_chain_hash(genesis_block.hash().into());
            try_init_contracts_context(
                genesis_block,
                fiber_config.scripts.clone(),
                ckb_config.udt_whitelist.clone().unwrap_or_default(),
            )
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

            let watchtower_actor = Actor::spawn_linked(
                Some("watchtower".to_string()),
                WatchtowerActor::new(store.clone()),
                ckb_config,
                root_actor.get_cell(),
            )
            .await
            .map_err(|err| ExitMessage(format!("failed to start watchtower actor: {}", err)))?
            .0;

            watchtower_actor.send_interval(
                Duration::from_secs(
                    fiber_config
                        .watchtower_check_interval_seconds
                        .unwrap_or(DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS),
                ),
                || WatchtowerMessage::PeriodicCheck,
            );

            #[cfg(debug_assertions)]
            let rpc_dev_module_commitment_txs_clone = rpc_dev_module_commitment_txs.clone();
            new_tokio_task_tracker().spawn(async move {
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
                                    // we may forward more events to the rpc dev module in the future for integration testing
                                    // for now, we only forward RemoteCommitmentSigned events, which are used for submitting outdated commitment transactions
                                    #[cfg(debug_assertions)]
                                    if let Some(rpc_dev_module_commitment_txs) = rpc_dev_module_commitment_txs_clone.as_ref() {
                                        if let NetworkServiceEvent::RemoteCommitmentSigned(_, channel_id, commitment_tx, _) = event.clone() {
                                            let lock_args = commitment_tx.outputs().get(0).unwrap().lock().args().raw_data();
                                            let version = u64::from_be_bytes(lock_args[28..36].try_into().unwrap());
                                            rpc_dev_module_commitment_txs.write().await.insert((channel_id, version), commitment_tx);
                                        }
                                    }
                                    // forward the event to the watchtower actor
                                    let _ = watchtower_actor.send_message(WatchtowerMessage::NetworkServiceEvent(event));
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

    let cch_actor = match config.cch {
        Some(cch_config) => {
            info!("Starting cch");
            let ignore_startup_failure = cch_config.ignore_startup_failure;
            match start_cch(
                cch_config,
                new_tokio_task_tracker(),
                new_tokio_cancellation_token(),
                root_actor.get_cell(),
                network_actor.clone(),
            )
            .await
            {
                Err(err) => {
                    if ignore_startup_failure {
                        info!("Cross-chain service failed to start and is ignored by the config option ignore_startup_failure: {}", err);
                        None
                    } else {
                        return ExitMessage::err(format!(
                            "cross-chain service failed to start: {}",
                            err
                        ));
                    }
                }
                Ok(actor) => {
                    subscribers.pending_received_tlcs_subscribers.subscribe(
                        actor.clone(),
                        |tlc_notification| {
                            Some(CchMessage::PendingReceivedTlcNotification(tlc_notification))
                        },
                    );
                    subscribers.settled_tlcs_subscribers.subscribe(
                        actor.clone(),
                        |tlc_notification| {
                            Some(CchMessage::SettledTlcNotification(tlc_notification))
                        },
                    );

                    Some(actor)
                }
            }
        }
        None => None,
    };

    // Start rpc service
    let rpc_server_handle = match (config.rpc, network_graph) {
        (Some(rpc_config), Some(network_graph)) => {
            let handle = start_rpc(
                rpc_config,
                config.ckb,
                config.fiber,
                network_actor,
                cch_actor,
                store,
                network_graph,
                #[cfg(debug_assertions)] ckb_chain_actor,
                #[cfg(debug_assertions)] rpc_dev_module_commitment_txs,
            )
            .await;
            Some(handle)
        },
        (Some(_), None) => return ExitMessage::err(
            "RPC requires network graph in the fiber service which is not enabled in the config file"
            .to_string()
        ),
        _ => None,
    };

    signal_listener().await;
    if let Some((handle, _)) = rpc_server_handle {
        handle
            .stop()
            .map_err(|err| ExitMessage(format!("failed to stop rpc server: {}", err)))?;
        handle.stopped().await;
    }
    cancel_tasks_and_wait_for_completion().await;

    Ok(())
}

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

#[cfg(target_family = "unix")]
async fn signal_listener() {
    use tokio::signal::unix::{signal, SignalKind};
    // SIGTERM is commonly sent for graceful shutdown of applications, followed by 30 seconds of grace time, then a SIGKILL.
    let mut sigterm = signal(SignalKind::terminate()).expect("listen for SIGTERM");
    // SIGINT is usually sent due to ctrl-c in the terminal.
    let mut sigint = signal(SignalKind::interrupt()).expect("listen for SIGINT");

    tokio::select! {
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
        _ = sigint.recv() => info!("SIGINT received, shutting down"),
    };
}

#[cfg(not(target_family = "unix"))]
async fn signal_listener() {
    tokio::signal::ctrl_c()
        .await
        .expect("listen for Ctrl-c signal");
    tracing::info!("Ctrl-c received, shutting down");
}
