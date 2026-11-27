use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};

use ckb_types::{
    core::tx_pool::TxStatus,
    packed::{CellDep, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Unpack},
};
use fiber_types::{Hash256, NodeId};
use ractor::{Actor, ActorRef};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::contracts::{get_cell_deps, get_cell_deps_count, get_script_by_contract, Contract};
use crate::ckb::signer::LocalSigner;
use crate::ckb::tests::test_utils::{MockChainActor, MockChainState, MockCkbChainClient};
use crate::ckb::CkbChainMessage;
use crate::fiber::graph::NetworkGraph;
use crate::fiber::onchain_tlc_reconcile::verify_and_select_settlement_data_with_superseded;
use crate::fiber::types::BroadcastMessageWithTimestamp;
use crate::fiber::{FiberConfig, Privkey};
use crate::signing::SigningEvents;
use crate::simulation::SimulatedChainAccess;
use crate::store::Store;
use crate::watchtower::{
    simulation::SimulatedWatchtowerChain, WatchtowerActor, WatchtowerMessage, WatchtowerStore,
};

use super::{NetworkActor, NetworkActorMessage, NetworkActorStartArguments, NetworkServiceEvent};

/// Chain state shared by all simulated nodes and retained across host crashes.
#[derive(Clone, Default)]
pub struct SimulatedChain(Arc<StdRwLock<MockChainState>>);

impl SimulatedChain {
    /// Start the real watchtower using in-memory CKB RPC. An observer with
    /// `can_submit = false` verifies witnesses without funding or spending cells.
    pub async fn start_watchtower(
        &self,
        store: Store,
        signer: Privkey,
        access: SimulatedChainAccess,
        can_submit: bool,
    ) -> anyhow::Result<(ActorRef<WatchtowerMessage>, tokio::task::JoinHandle<()>)> {
        let chain = SimulatedWatchtowerChain::new(self.0.clone(), access, signer, can_submit)?;
        let actor = WatchtowerActor::new(store).with_simulated_chain(chain);
        Ok(Actor::spawn(None, actor, Default::default()).await?)
    }

    /// Sum the live CKB in the test wallet that funds Watchtower fees and receives payouts.
    pub fn watchtower_wallet_capacity(&self, key: Privkey) -> u128 {
        let signer = LocalSigner::new(key.into());
        let lock = get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
        self.0
            .read()
            .unwrap()
            .live_cells()
            .into_iter()
            .filter(|cell| cell.output.lock() == lock && cell.output.type_().is_none())
            .map(|cell| u128::from(Unpack::<u64>::unpack(&cell.output.capacity())))
            .sum()
    }

    /// Return the first confirmed settlement spend of a commitment transaction.
    pub fn commitment_spend(
        &self,
        commitment: fiber_types::Hash256,
    ) -> Option<fiber_types::Hash256> {
        let outpoint = ckb_types::packed::OutPoint::new(commitment.into(), 0);
        self.confirmed_spend(&outpoint)
    }

    /// Return the confirmed spend of the channel's funding output.
    pub fn funding_spend(&self, funding_tx: Hash256) -> Option<Hash256> {
        self.confirmed_spend(&ckb_types::packed::OutPoint::new(funding_tx.into(), 0))
    }

    /// List confirmed settlements that consumed this commitment or its successor cells.
    pub fn commitment_settlements(&self, commitment: Hash256) -> BTreeSet<Hash256> {
        let state = self.0.read().unwrap();
        let mut settlements = BTreeSet::new();
        let Some(lock) = confirmed_commitment_lock(&state, commitment) else {
            return settlements;
        };
        let args = lock.args().raw_data();
        let Some(prefix) = args.get(..36) else {
            return settlements;
        };
        let mut outpoint = ckb_types::packed::OutPoint::new(commitment.into(), 0);
        while let Some((hash, tx)) = state.txs.iter().find_map(|(hash, record)| {
            let tx = record.transaction.as_ref()?;
            (matches!(record.tx_status, TxStatus::Committed(..))
                && tx.input_pts_iter().any(|input| input == outpoint))
            .then_some((*hash, tx))
        }) {
            settlements.insert(hash);
            let successor = tx.outputs().into_iter().position(|output| {
                let next = output.lock();
                next.code_hash() == lock.code_hash()
                    && next.hash_type() == lock.hash_type()
                    && next.args().raw_data().starts_with(prefix)
            });
            let Some(index) = successor else {
                break;
            };
            outpoint = ckb_types::packed::OutPoint::new(hash.into(), index as u32);
        }
        settlements
    }

    /// Check that this tower's durable snapshot matches the confirmed commitment.
    pub fn watchtower_matches_commitment(
        &self,
        store: &Store,
        channel_id: &Hash256,
        commitment: Hash256,
    ) -> bool {
        let Some(lock) = confirmed_commitment_lock(&self.0.read().unwrap(), commitment) else {
            return false;
        };
        let node = NodeId::local();
        let Some((_, channel)) = store
            .get_watch_channels_with_nodes()
            .into_iter()
            .find(|(owner, channel)| *owner == node && channel.channel_id == *channel_id)
        else {
            return false;
        };
        let superseded = store.get_superseded_remote_settlements(&node, channel_id);
        verify_and_select_settlement_data_with_superseded(&channel, &superseded, &lock).is_some()
    }

    /// Whether the commitment and all successor cells have been spent.
    pub fn commitment_settled(&self, commitment: fiber_types::Hash256) -> bool {
        let state = self.0.read().unwrap();
        let Some(commitment_lock) = confirmed_commitment_lock(&state, commitment) else {
            return false;
        };
        let args = commitment_lock.args().raw_data();
        let Some(prefix) = args.get(..36) else {
            return false;
        };
        !state.live_cells().into_iter().any(|cell| {
            let lock = cell.output.lock();
            lock.code_hash() == commitment_lock.code_hash()
                && lock.hash_type() == commitment_lock.hash_type()
                && lock.args().raw_data().starts_with(prefix)
        })
    }

    /// Return the confirmed transaction that consumed this cell.
    pub fn confirmed_spend(
        &self,
        outpoint: &ckb_types::packed::OutPoint,
    ) -> Option<fiber_types::Hash256> {
        self.0
            .read()
            .unwrap()
            .txs
            .iter()
            .find_map(|(hash, record)| {
                (matches!(record.tx_status, TxStatus::Committed(..))
                    && record
                        .transaction
                        .as_ref()
                        .is_some_and(|tx| tx.input_pts_iter().any(|input| &input == outpoint)))
                .then_some(*hash)
            })
    }

    /// List transactions that created an output with this funding lock and whether each committed.
    pub fn funding_transactions(&self, lock: &Script) -> BTreeMap<fiber_types::Hash256, bool> {
        self.0
            .read()
            .expect("shared simulated chain")
            .txs
            .iter()
            .filter(|(_, response)| {
                response.transaction.as_ref().is_some_and(|tx| {
                    tx.outputs()
                        .into_iter()
                        .any(|output| output.lock() == *lock)
                })
            })
            .map(|(hash, response)| (*hash, matches!(response.tx_status, TxStatus::Committed(..))))
            .collect()
    }

    /// Look up a live cell on the simulated chain for independent wallet verification.
    pub fn live_cell(&self, outpoint: &OutPoint) -> Option<(CellOutput, Vec<u8>)> {
        self.0
            .read()
            .unwrap()
            .live_cells()
            .into_iter()
            .find(|cell| &cell.out_point == outpoint)
            .map(|cell| (cell.output, cell.output_data.to_vec()))
    }

    /// Verify that `outpoint` is the source commitment or a confirmed settlement descendant.
    pub fn verify_commitment_lineage(
        &self,
        source: Hash256,
        outpoint: &OutPoint,
    ) -> Result<(), String> {
        let onchain_tx_hash = {
            let state = self.0.read().unwrap();
            if state
                .txs
                .get(&source)
                .is_some_and(|record| matches!(record.tx_status, TxStatus::Committed(..)))
            {
                Some(source)
            } else {
                // On-chain commitment transactions attach FundingLock cell_deps at broadcast time,
                // whereas SDK RecoveryRecord::reference::tx_hash is computed over the pre-broadcast
                // commitment template with empty cell_deps.
                state.txs.iter().find_map(|(hash, record)| {
                    if !matches!(record.tx_status, TxStatus::Committed(..)) {
                        return None;
                    }
                    let tx = record.transaction.as_ref()?;
                    let stripped = tx
                        .data()
                        .raw()
                        .as_builder()
                        .cell_deps(ckb_types::packed::CellDepVec::default())
                        .build();
                    (Hash256::from(stripped.calc_tx_hash()) == source).then_some(*hash)
                })
            }
        }
        .ok_or_else(|| format!("source commitment {source} is not committed"))?;

        let tx_hash: Hash256 = outpoint.tx_hash().into();
        if tx_hash == onchain_tx_hash
            || self
                .commitment_settlements(onchain_tx_hash)
                .contains(&tx_hash)
        {
            Ok(())
        } else {
            Err(format!(
                "outpoint tx {tx_hash} does not descend from commitment {source}"
            ))
        }
    }

    /// Cell dependencies used by simulated commitment settlement transactions.
    pub async fn settlement_cell_deps(&self) -> Vec<CellDep> {
        get_cell_deps(vec![Contract::CommitmentLock], &None)
            .await
            .expect("simulated settlement cell deps")
            .into_iter()
            .collect()
    }

    /// Trusted lock scripts and commitment dep count for SDK opening validation.
    pub fn opening_network_scripts(&self) -> (Script, Script, usize) {
        (
            get_script_by_contract(Contract::FundingLock, &[]),
            get_script_by_contract(Contract::CommitmentLock, &[]),
            get_cell_deps_count(vec![Contract::FundingLock], &None),
        )
    }

    /// Standard secp256k1 lock script for a simulated wallet key.
    pub fn wallet_lock_script(key: Privkey) -> Script {
        let signer = LocalSigner::new(key.into());
        get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash())
    }
}

fn confirmed_commitment_lock(state: &MockChainState, commitment: Hash256) -> Option<Script> {
    let record = state.txs.get(&commitment)?;
    if !matches!(record.tx_status, TxStatus::Committed(..)) {
        return None;
    }
    Some(record.transaction.as_ref()?.output(0)?.lock())
}

/// A Fiber data plane and its service events, owned by the current host runtime.
pub struct SimulatedNode {
    /// Commands and incoming peer messages use the regular Fiber mailbox.
    pub actor: ActorRef<NetworkActorMessage>,
    /// Resolves if the Fiber data plane exits.
    pub task: tokio::task::JoinHandle<()>,
    /// Service events emitted by the real channel, payment and network logic.
    pub events: mpsc::Receiver<NetworkServiceEvent>,
    /// Signed channel advertisements for delivery by the simulator.
    pub broadcasts: mpsc::UnboundedReceiver<Vec<BroadcastMessageWithTimestamp>>,
    /// The mock CKB command mailbox and supervisor of transaction-tracing callbacks.
    pub chain_actor: ActorRef<CkbChainMessage>,
    /// Shared signing-event publisher wired into the node and optional LSP runtime.
    pub signing_events: SigningEvents,
    pub(crate) chain_client: MockCkbChainClient,
}

impl Drop for SimulatedNode {
    fn drop(&mut self) {
        fn unlink_children(parent: ractor::ActorCell) {
            for child in parent.get_children() {
                child.unlink(parent.clone());
                unlink_children(child);
            }
        }
        // Runtime cancellation skips Ractor's supervision cleanup.
        unlink_children(self.actor.get_cell());
        unlink_children(self.chain_actor.get_cell());
    }
}

/// Start a node with supplied identity, entropy, database and shared mock chain.
/// The caller registers peers using the normal in-process connection commands.
pub async fn start_node(
    config: FiberConfig,
    private_key: Privkey,
    entropy: [u8; 32],
    store: Store,
    chain: SimulatedChain,
    access: SimulatedChainAccess,
) -> anyhow::Result<SimulatedNode> {
    anyhow::ensure!(
        crate::simulation::is_running(),
        "simulation environment missing"
    );
    let (chain_actor, _) = Actor::spawn(
        None,
        MockChainActor::new(),
        (Some(Box::new(access.clone()) as Box<_>), chain.0.clone()),
    )
    .await?;
    let mut chain_client = MockCkbChainClient::new(chain.0);
    chain_client.simulation_access = Some(access);
    let graph = NetworkGraph::new(store.clone(), private_key.pubkey(), true);
    let (sender, events) = mpsc::channel(512);
    let (broadcast, broadcasts) = mpsc::unbounded_channel();
    let signing_events = SigningEvents::default();
    let mut node = NetworkActor::new(
        sender,
        chain_actor.clone(),
        store,
        None,
        Arc::new(RwLock::new(graph)),
        chain_client.clone(),
    )
    .with_signing_events(signing_events.channels(None));
    node.core.simulation = Some(SimulatedTransport {
        key: private_key,
        entropy,
        broadcast,
    });
    let (actor, task) = Actor::spawn(
        None,
        node,
        NetworkActorStartArguments {
            config,
            tracker: Default::default(),
            default_shutdown_script: Default::default(),
        },
    )
    .await?;
    Ok(SimulatedNode {
        actor,
        task,
        events,
        broadcasts,
        chain_actor,
        signing_events,
        chain_client,
    })
}

/// Hosted LSP service and production Biscuit-authenticated JSON-RPC stack running in-memory.
#[cfg(feature = "lsp")]
pub struct SimulatedLspRuntime {
    pub lsp_actor: ActorRef<crate::lsp::LspServiceMessage>,
    pub admin_token: String,
    pub biscuit_public_key: String,
    methods: jsonrpsee::Methods,
    auth: Arc<crate::rpc::biscuit::BiscuitAuth>,
    _stop_handle: jsonrpsee::server::ServerHandle,
    stop_handle: jsonrpsee::server::StopHandle,
}

#[cfg(feature = "lsp")]
impl Drop for SimulatedLspRuntime {
    fn drop(&mut self) {
        fn unlink_children(parent: ractor::ActorCell) {
            for child in parent.get_children() {
                child.unlink(parent.clone());
                unlink_children(child);
            }
        }
        unlink_children(self.lsp_actor.get_cell());
    }
}

#[cfg(feature = "lsp")]
impl SimulatedLspRuntime {
    /// Execute a single raw JSON-RPC request through production `BiscuitAuthMiddleware`.
    pub async fn call_rpc(
        &self,
        bearer_token: Option<&str>,
        raw_request: &str,
    ) -> anyhow::Result<String> {
        use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
        use jsonrpsee::server::HttpBody;
        use jsonrpsee::ws_client::RpcServiceBuilder;
        use tower::Service;

        let mut req_builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            req_builder = req_builder.header(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
        }
        let req = req_builder.body(HttpBody::from(raw_request.to_owned()))?;
        let headers = req.headers().clone();
        let auth = self.auth.clone();
        let rpc_middleware = RpcServiceBuilder::new().layer_fn(move |service| {
            crate::rpc::middleware::BiscuitAuthMiddleware {
                headers: headers.clone(),
                inner: service,
                auth: auth.clone(),
                enable_auth: true,
            }
        });
        let mut svc = jsonrpsee::server::Server::builder()
            .to_service_builder()
            .set_rpc_middleware(rpc_middleware)
            .build(self.methods.clone(), self.stop_handle.clone());
        let response = svc
            .call(req)
            .await
            .map_err(|err| anyhow::anyhow!("in-memory LSP RPC service error: {err}"))?;
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| {
            hyper::body::Body::poll_frame(std::pin::Pin::new(&mut body), cx)
        })
        .await
        {
            let frame = frame.map_err(|err| anyhow::anyhow!("read LSP RPC body: {err}"))?;
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
            }
        }
        Ok(String::from_utf8(bytes)?)
    }
}

/// Start a hosted `LspService`, attach its delivery port to `node`, and assemble the
/// production Biscuit-authenticated JSON-RPC handlers for in-memory simulation dispatch.
#[cfg(feature = "lsp")]
#[allow(clippy::too_many_arguments)]
pub async fn start_lsp_runtime(
    node: &SimulatedNode,
    mut template_config: FiberConfig,
    private_key: Privkey,
    store: Store,
    lsp_dir: PathBuf,
    biscuit_seed: [u8; 32],
    mut lsp_config: crate::lsp::LspConfig,
) -> anyhow::Result<SimulatedLspRuntime> {
    use biscuit_auth::{macros::biscuit, Algorithm, KeyPair, PrivateKey};
    use jsonrpsee::RpcModule;

    use crate::fiber::network::PublicNetworkCommand;
    use crate::lsp::{
        BiscuitTokenIssuer, FiberTenantRuntimeFactory, LspDeliveryPort, LspService, LspServiceArgs,
        LspServiceMessage,
    };
    use crate::rpc::{
        biscuit::BiscuitAuth,
        channel::{ChannelRpcServer, ChannelRpcServerImpl},
        invoice::{InvoiceRpcServer, InvoiceRpcServerImpl},
        lsp::{LspRpcServer, LspRpcServerImpl},
        payment::{PaymentRpcServer, PaymentRpcServerImpl},
        watchtower::{WatchtowerRpcServer, WatchtowerRpcServerImpl},
    };
    use crate::store::NodeNamespace;

    std::fs::create_dir_all(&lsp_dir)?;
    let public_base_dir = lsp_dir.join("public");
    std::fs::create_dir_all(&public_base_dir)?;
    let sk_path = public_base_dir.join("sk");
    if !sk_path.exists() {
        let keypair = crate::fiber::KeyPair::try_from(private_key.as_ref().as_slice())
            .map_err(|err| anyhow::anyhow!("invalid LSP public keypair: {err}"))?;
        keypair.write_to_file(&sk_path)?;
    }
    template_config.base_dir = Some(public_base_dir);
    lsp_config.base_dir = Some(lsp_dir.clone());

    let biscuit_priv = PrivateKey::from_bytes(&biscuit_seed, Algorithm::Ed25519)
        .map_err(|err| anyhow::anyhow!("deterministic Biscuit key: {err}"))?;
    let biscuit_root = KeyPair::from(&biscuit_priv);
    let biscuit_public_key = biscuit_root.public().to_string();
    let token_issuer = BiscuitTokenIssuer::from_private_key(
        &biscuit_root.private().to_prefixed_string(),
        &biscuit_public_key,
    )?;

    let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
        node.signing_events.clone(),
        lsp_config.clone(),
        template_config.clone(),
        node.chain_client.clone(),
        node.chain_actor.clone(),
        node.actor.clone(),
        store.clone(),
        node.actor.get_cell(),
        Script::default(),
    ));
    let (lsp_actor, _) = Actor::spawn_linked(
        None,
        LspService,
        LspServiceArgs {
            config: lsp_config.clone(),
            public_node_id: private_key.pubkey(),
            public_network_actor: node.actor.clone(),
            store: store.namespaced(NodeNamespace::lsp_metadata()),
            runtime_factory,
            signing_key: private_key,
            token_issuer,
            watchtower_store: store.clone(),
            signing_events: node.signing_events.clone(),
        },
        node.actor.get_cell(),
    )
    .await?;

    node.actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetDeliveryPort(LspDeliveryPort::new(
                lsp_actor.clone(),
                store.clone(),
            )),
        ))
        .map_err(|err| anyhow::anyhow!("attach LSP delivery port: {err}"))?;

    let token_epochs = ractor::call!(lsp_actor, LspServiceMessage::GetTenantTokenEpochs)
        .map_err(|err| anyhow::anyhow!("query LSP token epochs: {err}"))?;
    let watchtower_quotas = ractor::call!(lsp_actor, LspServiceMessage::GetWatchtowerQuotas)
        .map_err(|err| anyhow::anyhow!("query LSP watchtower quotas: {err}"))?;

    let auth = Arc::new(
        BiscuitAuth::from_pubkey(biscuit_public_key.clone())?
            .with_lsp_enabled(true)
            .with_tenant_token_epochs(token_epochs),
    );
    let admin_token = biscuit!(
        r#"
            read("lsp");
            write("lsp");
            read("channels");
            write("channels");
            read("invoices");
            write("invoices");
            read("payments");
            write("payments");
            read("watchtower");
            write("watchtower");
        "#
    )
    .build(&biscuit_root)
    .map_err(|err| anyhow::anyhow!("build admin biscuit: {err}"))?
    .to_base64()
    .map_err(|err| anyhow::anyhow!("encode admin biscuit: {err}"))?;

    let mut modules = RpcModule::new(());
    modules.merge(
        InvoiceRpcServerImpl::new(
            store.clone(),
            Some(node.actor.clone()),
            Some(template_config),
        )
        .with_lsp_actor(Some(lsp_actor.clone()))
        .into_rpc(),
    )?;
    modules.merge(
        ChannelRpcServerImpl::new(node.actor.clone(), store.clone())
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(
        PaymentRpcServerImpl::new(node.actor.clone(), store.clone())
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(
        WatchtowerRpcServerImpl::new(store)
            .with_signing_events(node.signing_events.clone())
            .with_quotas(watchtower_quotas)
            .with_lsp_enabled(true)
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(LspRpcServerImpl::new(lsp_actor.clone()).into_rpc())?;

    let (stop_handle, server_handle) = jsonrpsee::server::stop_channel();
    Ok(SimulatedLspRuntime {
        lsp_actor,
        admin_token,
        biscuit_public_key,
        methods: modules.into(),
        auth,
        _stop_handle: server_handle,
        stop_handle,
    })
}

pub(super) struct SimulatedTransport {
    pub key: Privkey,
    pub entropy: [u8; 32],
    pub broadcast: mpsc::UnboundedSender<Vec<BroadcastMessageWithTimestamp>>,
}
