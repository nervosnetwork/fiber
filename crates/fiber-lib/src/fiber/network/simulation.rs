use std::sync::{Arc, RwLock as StdRwLock};

use ractor::{Actor, ActorRef};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::tests::test_utils::{
    MockChainActor, MockChainActorMiddleware, MockChainState, MockCkbChainClient,
};
use crate::ckb::CkbChainMessage;
use crate::fiber::graph::NetworkGraph;
use crate::fiber::types::BroadcastMessageWithTimestamp;
use crate::fiber::{FiberConfig, Privkey};
use crate::signing::SigningEvents;
use crate::simulation::SimulatedChainClientHook;
use crate::store::Store;

use super::{NetworkActor, NetworkActorMessage, NetworkActorStartArguments, NetworkServiceEvent};

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
    /// Shared mock CKB RPC client wired into the node and optional LSP runtime.
    pub chain_client: MockCkbChainClient,
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
#[allow(clippy::too_many_arguments)]
pub async fn start_node(
    config: FiberConfig,
    private_key: Privkey,
    entropy: [u8; 32],
    store: Store,
    chain_state: Arc<StdRwLock<MockChainState>>,
    chain_middleware: Box<dyn MockChainActorMiddleware>,
    client_hook: Arc<dyn SimulatedChainClientHook>,
) -> anyhow::Result<SimulatedNode> {
    anyhow::ensure!(
        crate::simulation::is_running(),
        "simulation environment missing"
    );
    let (chain_actor, _) = Actor::spawn(
        None,
        MockChainActor::new(),
        (Some(chain_middleware), chain_state.clone()),
    )
    .await?;
    let mut chain_client = MockCkbChainClient::new(chain_state);
    chain_client.simulation_hook = Some(client_hook);
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

pub(super) struct SimulatedTransport {
    pub key: Privkey,
    pub entropy: [u8; 32],
    pub broadcast: mpsc::UnboundedSender<Vec<BroadcastMessageWithTimestamp>>,
}
