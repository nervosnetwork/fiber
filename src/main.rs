use cfn_node::cch::CchMessage;
use cfn_node::ckb::channel::ChannelSubscribers;
use cfn_node::ckb_chain::contracts::init_contracts_context;
use cfn_node::store::Store;
use ractor::Actor;
use tentacle::multiaddr::Multiaddr;
use tokio::sync::mpsc;
use tokio::{select, signal};
use tracing::{debug, error, info, info_span, trace};
use tracing_subscriber::field::MakeExt;
use tracing_subscriber::{fmt, EnvFilter};

use std::str::FromStr;

use cfn_node::actors::RootActor;
use cfn_node::ckb::{NetworkActorCommand, NetworkActorMessage};
use cfn_node::ckb_chain::CkbChainActor;
use cfn_node::tasks::{
    cancel_tasks_and_wait_for_completion, new_tokio_cancellation_token, new_tokio_task_tracker,
};
use cfn_node::{start_cch, start_ckb, start_ldk, start_rpc, Config};
use core::default::Default;
use tracing_subscriber::fmt::format;

#[tokio::main]
pub async fn main() {
    // ractor will set "id" for each actor:
    // https://github.com/slawlor/ractor/blob/67d657e4cdcb8884a9ccc9b758704cbb447ac163/ractor/src/actor/mod.rs#L701
    // here we map it with the node prefix
    let node_formatter = format::debug_fn(|writer, field, value| {
        if field.name() == "id" {
            write!(
                writer,
                "{}: {:?} on {}",
                field,
                value,
                cfn_node::get_node_prefix()
            )
        } else {
            write!(writer, "{}: {:?}", field, value)
        }
    })
    .delimited(", ");

    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .pretty()
        .fmt_fields(node_formatter)
        .init();

    let _span = info_span!("node", node = cfn_node::get_node_prefix()).entered();

    let config = Config::parse();
    debug!("Parsed config: {:?}", &config);

    if let Some(ldk_config) = config.ldk {
        info!("Starting ldk");
        start_ldk(ldk_config).await;
    }

    let tracker = new_tokio_task_tracker();
    let token = new_tokio_cancellation_token();
    let root_actor = RootActor::start(tracker, token).await;

    let store = Store::new(config.ckb.as_ref().unwrap().store_path());
    let subscribers = ChannelSubscribers::default();

    let ckb_command_sender = match config.ckb {
        Some(ckb_config) => {
            // TODO: this is not a super user friendly error message which has actionable information
            // for the user to fix the error and start the node.
            let ckb_chain_config = config.ckb_chain.expect("ckb-chain service is required for ckb service. \
            Add ckb-chain service to the services list in the config file and relevant configuration to the ckb_chain section of the config file.");

            let _ = init_contracts_context(ckb_config.network, Some(&ckb_chain_config));

            let ckb_chain_actor = Actor::spawn_linked(
                Some("ckb-chain".to_string()),
                CkbChainActor {},
                ckb_chain_config,
                root_actor.get_cell(),
            )
            .await
            .expect("start ckb-chain actor")
            .0;

            const CHANNEL_SIZE: usize = 4000;
            let (event_sender, mut event_receiver) = mpsc::channel(CHANNEL_SIZE);

            let bootnodes = ckb_config.bootnode_addrs.clone();

            info!("Starting ckb");
            let ckb_actor = start_ckb(
                ckb_config,
                ckb_chain_actor,
                event_sender,
                new_tokio_task_tracker(),
                root_actor.get_cell(),
                store.clone(),
                subscribers.clone(),
            )
            .await;

            for bootnode in bootnodes {
                let addr = Multiaddr::from_str(&bootnode).expect("valid bootnode");
                let command = NetworkActorCommand::ConnectPeer(addr);
                ckb_actor
                    .send_message(NetworkActorMessage::new_command(command))
                    .expect("ckb actor alive")
            }

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
                                    trace!("Received event from ckb service: {:?}", event);
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

            Some(ckb_actor)
        }
        None => None,
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
                ckb_command_sender.clone(),
            )
            .await
            {
                Err(err) => {
                    error!("Cross-chain service failed to start: {}", err);
                    if ignore_startup_failure {
                        None
                    } else {
                        return;
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
    let rpc_server_handle = match config.rpc {
        Some(rpc_config) => {
            if ckb_command_sender.is_none() && cch_actor.is_none() {
                error!("Rpc service requires ckb and cch service to be started. Exiting.");
                return;
            }

            info!("Starting rpc");
            let handle = start_rpc(rpc_config, ckb_command_sender, cch_actor, store).await;
            Some(handle)
        }
        None => None,
    };

    signal::ctrl_c().await.expect("Failed to listen for event");
    info!("Received Ctrl-C, shutting down");
    if let Some(handle) = rpc_server_handle {
        handle.stop().unwrap();
        handle.stopped().await;
    }
    cancel_tasks_and_wait_for_completion().await;
}
