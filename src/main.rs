use ckb_chain_spec::ChainSpec;
use ckb_hash::blake2b_256;
use ckb_resource::Resource;
use core::default::Default;
use fnn::actors::RootActor;
use fnn::cch::CchMessage;
use fnn::ckb::{
    contracts::{get_script_by_contract, try_init_contracts_context, Contract},
    CkbChainActor,
};
use fnn::fiber::{channel::ChannelSubscribers, graph::NetworkGraph, network::init_chain_hash};
use fnn::store::Store;
use fnn::tasks::{
    cancel_tasks_and_wait_for_completion, new_tokio_cancellation_token, new_tokio_task_tracker,
};
use fnn::watchtower::{WatchtowerActor, WatchtowerMessage};
use fnn::{start_cch, start_network, start_rpc, Config};
use ractor::Actor;
use secp256k1::Secp256k1;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::{select, signal};
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

    info!("Starting node with git version {}", fnn::get_git_versin());

    let _span = info_span!("node", node = fnn::get_node_prefix()).entered();

    let (config, run_migrate) = Config::parse();

    let store_path = config
        .fiber
        .as_ref()
        .ok_or_else(|| ExitMessage("fiber config is required but absent".to_string()))?
        .store_path();
    if run_migrate {
        Store::run_migrate(store_path).map_err(|err| ExitMessage(err.to_string()))?;
        return Ok(());
    }
    let store = Store::new(store_path).map_err(|err| ExitMessage(err.to_string()))?;

    let tracker = new_tokio_task_tracker();
    let token = new_tokio_cancellation_token();
    let root_actor = RootActor::start(tracker, token).await;
    let subscribers = ChannelSubscribers::default();

    let (fiber_command_sender, network_graph) = match config.fiber.clone() {
        Some(fiber_config) => {
            // TODO: this is not a super user friendly error message which has actionable information
            // for the user to fix the error and start the node.
            let ckb_config = config.ckb.ok_or_else(|| {
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

            let ckb_actor = Actor::spawn_linked(
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
            )));

            let secret_key = ckb_config.read_secret_key().map_err(|err| {
                ExitMessage(format!(
                    "failed to read the secret key for the ckb signer: {}",
                    err
                ))
            })?;
            let secp = Secp256k1::new();
            let pubkey_hash = blake2b_256(secret_key.public_key(&secp).serialize());
            let default_shutdown_script =
                get_script_by_contract(Contract::Secp256k1Lock, &pubkey_hash[0..20]);

            info!("Starting fiber");
            let network_actor = start_network(
                fiber_config,
                ckb_actor,
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

            // every 60 seconds, check if there are any channels that submitted a commitment transaction
            // TODO: move interval to config file
            watchtower_actor
                .send_interval(Duration::from_secs(60), || WatchtowerMessage::PeriodicCheck);

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

            (Some(network_actor), Some(network_graph))
        }
        None => (None, None),
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
                fiber_command_sender.clone(),
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
                config.fiber,
                fiber_command_sender,
                cch_actor,
                store,
                network_graph
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

    signal::ctrl_c()
        .await
        .map_err(|err| ExitMessage(format!("failed to listen for ctrl-c event: {}", err)))?;
    info!("Received Ctrl-C, shutting down");
    if let Some(handle) = rpc_server_handle {
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
