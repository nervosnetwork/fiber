use std::cell::RefCell;
use std::future;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use crate::chain::SimulatedChain;
use crate::chain_fault::SimulatedChainAccess;
use anyhow::{bail, Context, Result};
use fnn::event_handler::forward_event_to_client;
use fnn::fiber::{NetworkServiceEvent, Privkey};
use fnn::fiber_types::NodeId;
use fnn::rpc::watchtower::{RpcContext, WatchtowerRpcServer, WatchtowerRpcServerImpl};
use fnn::store::{open_store_with_migration, Store};
use fnn::watchtower::{WatchtowerMessage, WatchtowerStore};
use jsonrpsee::core::client::{BatchResponse, ClientT, Error as ClientError};
use jsonrpsee::core::params::BatchRequestBuilder;
use jsonrpsee::core::traits::ToRpcParams;
use ractor::ActorRef;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use turmoil::net::{TcpListener, TcpStream};

use crate::host::{record, Trace, NAMES};

const RPC_PORT: u16 = 7200;
const RPC_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_RPC_FRAME: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TowerMode {
    BuiltIn,
    External,
}

const EXTERNAL_HOSTS: [&str; 8] = ["W-A", "W-B", "W-C", "W-D", "W-E", "W-F", "W-G", "W-H"];

#[derive(Clone)]
pub(crate) struct Tower {
    pub mode: TowerMode,
    pub enabled: watch::Sender<bool>,
    pub actor: Rc<RefCell<Option<ActorRef<WatchtowerMessage>>>>,
    pub store: Rc<RefCell<Option<Store>>>,
    pub access: SimulatedChainAccess,
    owner: usize,
    preimage_failures: Arc<AtomicU64>,
    trace: Trace,
}

impl Tower {
    pub fn new(owner: usize, mode: TowerMode, trace: Trace) -> Self {
        Self {
            mode,
            owner,
            preimage_failures: Default::default(),
            trace,
            enabled: watch::Sender::new(false),
            actor: Default::default(),
            store: Default::default(),
            access: Default::default(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self.mode {
            TowerMode::BuiltIn => NAMES[self.owner],
            TowerMode::External => EXTERNAL_HOSTS[self.owner],
        }
    }

    pub fn preimage_failures(&self) -> u64 {
        self.preimage_failures.load(Ordering::Relaxed)
    }

    pub async fn run(&self, store: Store, chain: SimulatedChain, key: Privkey) -> Result<()> {
        self.store.replace(Some(store.clone()));
        let mut enabled = self.enabled.subscribe();
        loop {
            enabled.wait_for(|active| *active).await?;
            let (actor, mut task) = chain
                .start_watchtower(store.clone(), key.clone(), self.access.clone(), true)
                .await?;
            self.actor.replace(Some(actor.clone()));
            record(
                &self.trace,
                format_args!("{} watchtower started", self.name()),
            );

            tokio::select! {
                result = &mut task => {
                    result.context("watchtower task failed")?;
                    bail!("{} watchtower exited", self.name());
                }
                result = enabled.wait_for(|active| !*active) => { result?; }
            }
            actor.kill();
            task.await.context("watchtower task failed")?;
            self.actor.take();
            record(
                &self.trace,
                format_args!("{} watchtower stopped", self.name()),
            );
        }
    }

    pub async fn run_external(
        &self,
        path: PathBuf,
        chain: SimulatedChain,
        key: Privkey,
    ) -> Result<()> {
        let store = open_store_with_migration(
            &path,
            true,
            fnn::fiber::config::DEFAULT_STORE_CHANNEL_CACHE_CAPACITY,
            Box::new(|_| true),
            Box::new(|_| {}),
        )
        .map_err(|error| anyhow::anyhow!(error))?;
        tokio::try_join!(self.run(store.clone(), chain, key), self.serve_rpc(store),)?;
        Ok(())
    }

    async fn serve_rpc(&self, store: Store) -> Result<()> {
        let module = WatchtowerRpcServerImpl::new(store).into_rpc();
        let context = json!(RpcContext {
            node_id: NodeId::local().to_string(),
            tenant_scoped: false,
        });
        let listener = TcpListener::bind(("0.0.0.0", RPC_PORT)).await?;
        let mut requests = JoinSet::new();
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (mut socket, _) = result?;
                    let module = module.clone();
                    let context = context.clone();
                    requests.spawn_local(async move {
                        // Bound the lifetime of half-open requests and lost responses.
                        let _ = tokio::time::timeout(RPC_TIMEOUT, async {
                            let mut request: Value = serde_json::from_str(&read_message(&mut socket).await?)?;
                            // Mirror the context injection of the local RPC middleware.
                            let params = request["params"].get(0).cloned().context("missing watchtower RPC params")?;
                            request["params"] = json!([context, params]);
                            let (response, _) = module.raw_json_request(&request.to_string(), 1).await?;
                            write_message(&mut socket, response.get()).await?;
                            Ok::<_, anyhow::Error>(())
                        }).await;
                    });
                }
                Some(result) = requests.join_next() => { result?; }
            }
        }
    }

    pub async fn forward(
        &self,
        mut events: mpsc::UnboundedReceiver<NetworkServiceEvent>,
    ) -> Result<()> {
        let client = TowerClient {
            destination: self.name(),
            preimage_failures: self.preimage_failures.clone(),
        };
        while let Some(event) = events.recv().await {
            if let Err(error) = forward_event_to_client(event, &client).await {
                record(
                    &self.trace,
                    format_args!(
                        "{} failed to forward event to external watchtower: {error}",
                        NAMES[self.owner],
                    ),
                );
            }
        }
        future::pending().await
    }
}

/// Persist the same watcher events delivered to a built-in Watchtower.
pub(crate) fn persist_event(event: &NetworkServiceEvent, store: &Store) {
    let node = NodeId::local();
    match event {
        NetworkServiceEvent::RemoteTxComplete(_, channel, udt, keys, settlement, features) => {
            store.insert_watch_channel(
                node,
                *channel,
                udt.clone(),
                keys.clone(),
                settlement.clone(),
                *features,
            );
        }
        NetworkServiceEvent::ChannelClosed(_, channel, _)
        | NetworkServiceEvent::ChannelAbandon(channel)
        | NetworkServiceEvent::ChannelFundingAborted(channel) => {
            store.remove_watch_channel(node, *channel);
        }
        NetworkServiceEvent::RevokeAndAckReceived(_, channel, revocation, settlement) => {
            store.update_revocation(node, *channel, revocation.clone(), settlement.clone());
        }
        NetworkServiceEvent::LocalCommitmentSigned(channel, number, settlement) => {
            let _ =
                store.update_pending_remote_settlement(node, *channel, *number, settlement.clone());
        }
        NetworkServiceEvent::RemoteCommitmentSigned(_, channel, _, settlement) => {
            store.update_local_settlement(node, *channel, settlement.clone());
        }
        NetworkServiceEvent::PreimageCreated(payment, preimage)
            if fnn::fiber_types::HashAlgorithm::supported_algorithms()
                .iter()
                .any(|algorithm| *payment == algorithm.hash(*preimage).into()) =>
        {
            store.insert_watch_preimage(node, *payment, *preimage);
        }
        NetworkServiceEvent::PreimageRemoved(payment) => {
            store.remove_watch_preimage(node, *payment);
        }
        _ => {}
    }
}

/// Carry Fiber's existing JSON-RPC methods over a length-prefixed Turmoil TCP stream.
struct TowerClient {
    destination: &'static str,
    preimage_failures: Arc<AtomicU64>,
}

impl ClientT for TowerClient {
    async fn request<R: DeserializeOwned, Params: ToRpcParams + Send>(
        &self,
        method: &str,
        params: Params,
    ) -> Result<R, ClientError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params.to_rpc_params()?,
        })
        .to_string();
        let result = tokio::time::timeout(RPC_TIMEOUT, async {
            let mut socket = TcpStream::connect((self.destination, RPC_PORT)).await?;
            write_message(&mut socket, &payload).await?;
            read_message(&mut socket).await
        })
        .await;
        let response = result
            .map_err(|_| ClientError::RequestTimeout)
            .and_then(|result| result.map_err(|error| ClientError::Transport(Box::new(error))));
        if response.is_err() && method == "create_preimage" {
            self.preimage_failures.fetch_add(1, Ordering::Relaxed);
        }
        let response = response?;
        let response: Value = serde_json::from_str(&response)?;
        if let Some(error) = response.get("error") {
            return Err(ClientError::Call(serde_json::from_value(error.clone())?));
        }
        let result = response
            .get("result")
            .context("watchtower RPC response missing result")
            .map_err(|error| ClientError::Custom(error.to_string()))?;
        Ok(serde_json::from_value(result.clone())?)
    }

    async fn notification<Params: ToRpcParams + Send>(
        &self,
        _: &str,
        _: Params,
    ) -> Result<(), ClientError> {
        Err(ClientError::HttpNotImplemented)
    }

    async fn batch_request<'a, R>(
        &self,
        _: BatchRequestBuilder<'a>,
    ) -> Result<BatchResponse<'a, R>, ClientError>
    where
        R: DeserializeOwned + std::fmt::Debug + 'a,
    {
        Err(ClientError::HttpNotImplemented)
    }
}

async fn write_message(socket: &mut TcpStream, value: &str) -> io::Result<()> {
    if value.len() > MAX_RPC_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized watchtower RPC message",
        ));
    }
    socket.write_u32(value.len() as u32).await?;
    socket.write_all(value.as_bytes()).await
}

async fn read_message(socket: &mut TcpStream) -> io::Result<String> {
    let length = socket.read_u32().await? as usize;
    if length > MAX_RPC_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized watchtower RPC message",
        ));
    }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
