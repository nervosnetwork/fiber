//! Shared in-memory CKB chain wrapper and inspection helpers for Fiber DST.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock as StdRwLock};

use ckb_types::{
    core::tx_pool::TxStatus,
    packed::{CellDep, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Unpack},
};
use fnn::ckb::contracts::{get_cell_deps, get_cell_deps_count, get_script_by_contract, Contract};
use fnn::ckb::signer::LocalSigner;
use fnn::ckb::tests::test_utils::MockChainState;
use fnn::fiber::onchain_tlc_reconcile::verify_and_select_settlement_data_with_superseded;
use fnn::fiber::{FiberConfig, Privkey};
use fnn::fiber_types::{Hash256, NodeId};
use fnn::simulation::SimulatedNode;
use fnn::store::Store;
use fnn::watchtower::{WatchtowerActor, WatchtowerMessage, WatchtowerStore};
use ractor::{Actor, ActorRef};

use crate::chain_fault::SimulatedChainAccess;
use crate::watchtower_chain::{live_cells, SimulatedChainClock, SimulatedWatchtowerChain};

/// Chain state shared by all simulated nodes and retained across host crashes.
#[derive(Clone)]
pub struct SimulatedChain {
    state: Arc<StdRwLock<MockChainState>>,
    clock: Arc<SimulatedChainClock>,
}

impl Default for SimulatedChain {
    fn default() -> Self {
        let clock = Arc::new(SimulatedChainClock::default());
        let mut state = MockChainState::default();
        state.driver = Some(clock.clone());
        Self {
            state: Arc::new(StdRwLock::new(state)),
            clock,
        }
    }
}

impl SimulatedChain {
    /// Start a Fiber node connected to this shared simulated chain.
    pub async fn start_node(
        &self,
        config: FiberConfig,
        private_key: Privkey,
        entropy: [u8; 32],
        store: Store,
        access: SimulatedChainAccess,
    ) -> anyhow::Result<SimulatedNode> {
        fnn::simulation::start_node(
            config,
            private_key,
            entropy,
            store,
            self.state.clone(),
            Box::new(access.clone()),
            Arc::new(access),
        )
        .await
    }

    /// Start the real watchtower using in-memory CKB RPC. An observer with
    /// `can_submit = false` verifies witnesses without funding or spending cells.
    pub async fn start_watchtower(
        &self,
        store: Store,
        signer: Privkey,
        access: SimulatedChainAccess,
        can_submit: bool,
    ) -> anyhow::Result<(ActorRef<WatchtowerMessage>, tokio::task::JoinHandle<()>)> {
        let chain = Arc::new(SimulatedWatchtowerChain::new(
            self.state.clone(),
            self.clock.clone(),
            access,
            signer,
            can_submit,
        )?);
        let actor = WatchtowerActor::new(store).with_simulated_chain(chain);
        Ok(Actor::spawn(None, actor, Default::default()).await?)
    }

    /// Sum the live CKB in the test wallet that funds Watchtower fees and receives payouts.
    pub fn watchtower_wallet_capacity(&self, key: Privkey) -> u128 {
        let signer = LocalSigner::new(key.into());
        let lock = get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
        live_cells(&self.state.read().unwrap())
            .into_iter()
            .filter(|cell| cell.output.lock() == lock && cell.output.type_().is_none())
            .map(|cell| u128::from(Unpack::<u64>::unpack(&cell.output.capacity())))
            .sum()
    }

    /// Return the first confirmed settlement spend of a commitment transaction.
    pub fn commitment_spend(&self, commitment: Hash256) -> Option<Hash256> {
        let outpoint = OutPoint::new(commitment.into(), 0);
        self.confirmed_spend(&outpoint)
    }

    /// Return the confirmed spend of the channel's funding output.
    pub fn funding_spend(&self, funding_tx: Hash256) -> Option<Hash256> {
        self.confirmed_spend(&OutPoint::new(funding_tx.into(), 0))
    }

    /// List confirmed settlements that consumed this commitment or its successor cells.
    pub fn commitment_settlements(&self, commitment: Hash256) -> BTreeSet<Hash256> {
        let state = self.state.read().unwrap();
        let mut settlements = BTreeSet::new();
        let Some(lock) = confirmed_commitment_lock(&state, commitment) else {
            return settlements;
        };
        let args = lock.args().raw_data();
        let Some(prefix) = args.get(..36) else {
            return settlements;
        };
        let mut outpoint = OutPoint::new(commitment.into(), 0);
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
            outpoint = OutPoint::new(hash.into(), index as u32);
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
        let Some(lock) = confirmed_commitment_lock(&self.state.read().unwrap(), commitment) else {
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
    pub fn commitment_settled(&self, commitment: Hash256) -> bool {
        let state = self.state.read().unwrap();
        let Some(commitment_lock) = confirmed_commitment_lock(&state, commitment) else {
            return false;
        };
        let args = commitment_lock.args().raw_data();
        let Some(prefix) = args.get(..36) else {
            return false;
        };
        !live_cells(&state).into_iter().any(|cell| {
            let lock = cell.output.lock();
            lock.code_hash() == commitment_lock.code_hash()
                && lock.hash_type() == commitment_lock.hash_type()
                && lock.args().raw_data().starts_with(prefix)
        })
    }

    /// Return the confirmed transaction that consumed this cell.
    pub fn confirmed_spend(&self, outpoint: &OutPoint) -> Option<Hash256> {
        self.state
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
    pub fn funding_transactions(&self, lock: &Script) -> BTreeMap<Hash256, bool> {
        self.state
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
        live_cells(&self.state.read().unwrap())
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
            let state = self.state.read().unwrap();
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
