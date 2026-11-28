use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use anyhow::{ensure, Context, Result};
use fnn::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use fnn::fiber::network::{AcceptChannelCommand, AcceptChannelResponse, OpenChannelCommand};
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256};
use fnn::fiber_types::schema::CHANNEL_ACTOR_STATE_PREFIX;
use fnn::simulation;
use fnn::store::Store;
use tokio::sync::oneshot;

use crate::harness::Harness;
use crate::host::NAMES;

use super::{Driver, FUNDING_AMOUNT};

mod parallel;
mod serial;

#[derive(Debug)]
pub(super) struct Edge {
    pub sender: usize,
    pub receiver: usize,
    pub channel: Hash256,
    pub before: [u128; 2],
    pub funding_tx: Hash256,
    pub fee: u128,
}

impl Edge {
    pub(super) fn expected(&self, amount: u128) -> Result<[u128; 2]> {
        Ok([
            self.before[0]
                .checked_sub(amount)
                .context("path overspent its sending balance")?,
            self.before[1]
                .checked_add(amount)
                .context("receiving balance overflow")?,
        ])
    }
}

pub(super) struct FundingOrder {
    pub opener: usize,
    pub acceptor: usize,
    pub amount: u128,
    pub gap_ticks: usize,
}

#[derive(Clone, Copy)]
pub(super) enum BatchFault {
    None,
    DropUpdate(usize),
    AbortCleanup(usize),
    Chaos(usize),
    SlowChain { index: usize, ticks: u32 },
}

#[derive(Clone, Copy)]
pub(super) enum OpeningFault {
    None,
    CkbOutage,
    DropAcceptorFrame,
}

#[derive(Clone, Copy)]
enum Outcome {
    Online,
    Aborted,
}

pub(super) fn mutable_key(channel: Hash256) -> Vec<u8> {
    [&[CHANNEL_ACTOR_STATE_PREFIX], channel.as_ref()].concat()
}

fn audited_channel_state(
    store: &Store,
    id: Hash256,
    peer: &fnn::fiber::Pubkey,
) -> Result<Option<ChannelState>> {
    let state = simulation::channel_records(store, &id).map(|records| records.decode());
    let mut peer_rows = store
        .get_channel_states(None)
        .into_iter()
        .filter(|(_, channel, _)| *channel == id);
    let expected = state.as_ref().map(|state| (*peer, id, state.state));
    let indexed = peer_rows.next();
    ensure!(
        indexed == expected && peer_rows.next().is_none(),
        "channel {id} peer index={indexed:?}, expected={expected:?}"
    );
    let indexed = simulation::indexed_funding_outpoints(store, &id);
    let expected = state
        .as_ref()
        .and_then(|state| state.get_funding_transaction_outpoint());
    ensure!(
        indexed.iter().eq(expected.iter()),
        "channel {id} funding outpoint index={indexed:?}, expected={expected:?}"
    );
    if state.is_none() {
        ensure!(
            simulation::cached_channel_matches_disk(store, &id).is_none(),
            "deleted channel {id} remains in cache"
        );
    }
    simulation::verify_payment_hash_index(store).map_err(|error| anyhow::anyhow!(error))?;
    Ok(state.map(|state| state.state))
}

struct Attempt {
    order: FundingOrder,
    temporary: Hash256,
    channel: Hash256,
    // The acceptor has both funding keys before either side broadcasts.
    initial: Rc<ChannelActorState>,
}

impl Attempt {
    fn new(
        runtime: &Harness,
        order: FundingOrder,
        temporary: Hash256,
        channel: Hash256,
    ) -> Result<Self> {
        let initial = runtime
            .channel(order.acceptor, &channel)
            .with_context(|| format!("acceptor has no state for funding {channel}"))?;
        Ok(Self {
            order,
            temporary,
            channel,
            initial,
        })
    }

    fn aborted_on(&self, runtime: &Harness, node: usize) -> bool {
        runtime.funding_aborted(node, self.channel) || runtime.funding_aborted(node, self.temporary)
    }

    fn outcome(&self, runtime: &Harness) -> Option<Outcome> {
        let FundingOrder {
            opener, acceptor, ..
        } = self.order;
        if runtime.channel_online(opener, acceptor, &self.channel) {
            Some(Outcome::Online)
        } else if [opener, acceptor]
            .into_iter()
            .all(|node| self.aborted_on(runtime, node) && runtime.node_available(node))
        {
            Some(Outcome::Aborted)
        } else {
            None
        }
    }

    fn verify(&self, runtime: &Harness, outcome: Outcome, fee: u128) -> Result<Option<Edge>> {
        let channel = self.channel;
        let FundingOrder {
            opener,
            acceptor,
            amount,
            ..
        } = self.order;
        let lock = self.initial.get_funding_lock_script();
        let transactions = runtime.chain.funding_transactions(&lock);
        if matches!(outcome, Outcome::Aborted) {
            for (owner, peer) in [(opener, acceptor), (acceptor, opener)] {
                let store = runtime.store(owner)?;
                for id in [channel, self.temporary] {
                    ensure!(
                        audited_channel_state(&store, id, &runtime.peers[peer])?.is_none(),
                        "aborted channel {id} remains on {}",
                        NAMES[owner]
                    );
                }
            }
            ensure!(
                transactions.is_empty(),
                "aborted channel {channel} left funding transactions: {transactions:?}"
            );
            return Ok(None);
        }

        ensure!(
            [opener, acceptor]
                .into_iter()
                .all(|node| !self.aborted_on(runtime, node)),
            "online channel {channel} has an abort receipt"
        );
        let a = runtime
            .channel(opener, &channel)
            .context("missing opened channel on opener")?;
        let b = runtime
            .channel(acceptor, &channel)
            .context("missing opened channel on acceptor")?;
        let outpoint = a
            .get_funding_transaction_outpoint()
            .context("missing funding outpoint")?;
        let funding_tx = outpoint.tx_hash().into();
        ensure!(
            a.get_funding_lock_script() == lock
                && b.get_funding_lock_script() == lock
                && b.get_funding_transaction_outpoint().as_ref() == Some(&outpoint)
                && transactions == BTreeMap::from([(funding_tx, true)]),
            "channel {channel} has divergent or reused funding: {transactions:?}"
        );
        let before = [a.get_local_balance(), b.get_local_balance()];
        ensure!(
            before[0] + u128::from(a.local_reserved_ckb_amount) == amount
                && before[1] + u128::from(b.local_reserved_ckb_amount) == amount
                && a.get_remote_balance() == before[1]
                && b.get_remote_balance() == before[0]
                && !a.any_tlc_pending()
                && !b.any_tlc_pending(),
            "channel {channel} has invalid initial balances or pending TLCs"
        );
        Ok(Some(Edge {
            sender: opener,
            receiver: acceptor,
            channel,
            before,
            funding_tx,
            fee,
        }))
    }
}

impl Driver {
    fn protect_open(&mut self, a: usize, b: usize) -> Result<()> {
        // Drain endpoint faults before sending a new round of open/accept RPCs.
        if let Some(chaos) = &mut self.chaos {
            chaos.protect_open(a, b);
        }
        self.until("quiet initial TCP connection", |s| {
            let fault_drained = s.chaos.as_ref().is_none_or(|chaos| chaos.open_ready(a, b));
            Ok((fault_drained && s.runtime.link(a, b)).then_some(()))
        })
    }

    fn begin_open(
        &mut self,
        a: usize,
        b: usize,
        amount: u128,
        fee: u128,
        public: bool,
    ) -> Result<Hash256> {
        let payee = self.runtime.peers[b];
        let reply = self.runtime.request(a, |reply| {
            FiberActorCommand::OpenChannel(
                OpenChannelCommand {
                    pubkey: payee,
                    funding_amount: amount,
                    public,
                    one_way: false,
                    shutdown_script: None,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    commitment_delay_epoch: None,
                    funding_fee_rate: None,
                    tlc_expiry_delta: None,
                    tlc_min_value: None,
                    tlc_fee_proportional_millionths: Some(fee),
                    max_tlc_value_in_flight: None,
                    max_tlc_number_in_flight: None,
                },
                reply,
            )
        })?;
        let temporary = self.reply("open channel", reply)?.channel_id;
        self.until("acceptance request", |s| {
            Ok(s.runtime.nodes.borrow()[b]
                .pending_channels
                .contains(&temporary)
                .then_some(()))
        })?;
        Ok(temporary)
    }

    fn accept_open(
        &self,
        b: usize,
        temporary: Hash256,
        amount: u128,
    ) -> Result<oneshot::Receiver<Result<AcceptChannelResponse, String>>> {
        self.runtime.request(b, |reply| {
            FiberActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: temporary,
                    funding_amount: amount,
                    shutdown_script: None,
                    max_tlc_value_in_flight: None,
                    max_tlc_number_in_flight: None,
                    min_tlc_value: None,
                    tlc_fee_proportional_millionths: Some(0),
                    tlc_expiry_delta: None,
                },
                reply,
            )
        })
    }

    fn allow_open_faults(&mut self, a: usize, b: usize, channel: Hash256) -> Result<()> {
        if let Some(mut chaos) = self.chaos.take() {
            let result = chaos.after_accept(&mut self.runtime, a, b, channel);
            self.chaos = Some(chaos);
            result?;
        }
        Ok(())
    }

    fn finish_open_faults(&mut self) {
        if let Some(mut chaos) = self.chaos.take() {
            chaos.finish_open(&self.runtime);
            self.chaos = Some(chaos);
        }
    }

    pub(super) fn opened_edge(
        &self,
        sender: usize,
        receiver: usize,
        channel: Hash256,
        fee: u128,
    ) -> Result<Edge> {
        let a = self
            .runtime
            .channel(sender, &channel)
            .context("missing sender state")?;
        let b = self
            .runtime
            .channel(receiver, &channel)
            .context("missing receiver state")?;
        Ok(Edge {
            sender,
            receiver,
            channel,
            before: [a.get_local_balance(), b.get_local_balance()],
            funding_tx: a
                .get_funding_transaction_outpoint()
                .context("missing funding outpoint")?
                .tx_hash()
                .into(),
            fee,
        })
    }

    pub(super) fn check_funded_pair(
        &self,
        a: usize,
        b: usize,
        edges: &[Edge],
        funding_txs: &mut BTreeSet<Hash256>,
    ) -> Result<()> {
        let expected: BTreeSet<_> = edges.iter().map(|edge| edge.channel).collect();
        for (owner, peer) in [(a, b), (b, a)] {
            let nodes = self.runtime.nodes.borrow();
            let store = &nodes[owner]
                .node
                .as_ref()
                .context("funding peer offline")?
                .store;
            let persisted: BTreeSet<_> = store
                .get_channel_ids_by_pubkey(&self.runtime.peers[peer])
                .into_iter()
                .collect();
            ensure!(
                persisted == expected,
                "{} has funded channels {persisted:?}, expected {expected:?}",
                NAMES[owner]
            );
        }
        for edge in edges {
            ensure!(
                funding_txs.insert(edge.funding_tx),
                "channel {} reused funding transaction {}",
                edge.channel,
                edge.funding_tx
            );
        }
        Ok(())
    }
}
