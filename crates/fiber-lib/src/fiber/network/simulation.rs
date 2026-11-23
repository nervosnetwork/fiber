use std::collections::BTreeMap;
use std::sync::{Arc, RwLock as StdRwLock};

use ckb_types::{core::tx_pool::TxStatus, packed::Script, prelude::Unpack};
use fiber_types::{Hash256, NodeId};
use ractor::{Actor, ActorRef};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::ckb::signer::LocalSigner;
use crate::ckb::tests::test_utils::{MockChainActor, MockChainState, MockCkbChainClient};
use crate::ckb::CkbChainMessage;
use crate::fiber::graph::NetworkGraph;
use crate::fiber::onchain_tlc_reconcile::verify_and_select_settlement_data_with_superseded;
use crate::fiber::types::BroadcastMessageWithTimestamp;
use crate::fiber::{FiberConfig, Privkey};
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
    let mut node = NetworkActor::new(
        sender,
        chain_actor.clone(),
        store,
        None,
        Arc::new(RwLock::new(graph)),
        chain_client,
    );
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
    })
}

pub(super) struct SimulatedTransport {
    pub key: Privkey,
    pub entropy: [u8; 32],
    pub broadcast: mpsc::UnboundedSender<Vec<BroadcastMessageWithTimestamp>>,
}
