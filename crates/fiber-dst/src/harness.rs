use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, ensure, Context, Result};
use fnn::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use fnn::fiber::gossip::GossipMessageUpdates;
use fnn::fiber::graph::NetworkGraphStateStore;
use fnn::fiber::payment::PaymentSessionExt;
use fnn::fiber::{
    ChannelConnectivityState, ChannelState, Hash256, NetworkActorCommand, NetworkActorEvent,
    NetworkActorMessage, PaymentStatus, Privkey, Pubkey,
};
use fnn::simulation::{self, ChainFaultHit, SimulatedChain, SimulatedChainAccess, SimulatedDisk};
use ractor::RpcReplyPort;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::sync::oneshot;

use crate::host::{send, Host, LinkControl, Nodes, Peer, Slot, Trace, NAMES};

use crate::tower::{Tower, TowerMode};

const EPOCH: Duration = Duration::from_secs(1_700_000_000);
pub(crate) const TICK: Duration = Duration::from_millis(5);

pub(crate) struct Harness {
    world: turmoil::Sim<'static>,
    pub(crate) nodes: Nodes,
    pub(crate) peers: Vec<Pubkey>,
    trace: Trace,
    snapshots: RefCell<BTreeMap<(usize, Hash256), ChannelSnapshot>>,
    pub(crate) links: BTreeMap<(usize, usize), LinkControl>,
    pub(crate) chain: SimulatedChain,
    pub(crate) chain_access: Vec<SimulatedChainAccess>,
    pub(crate) chain_hits: Vec<Vec<ChainFaultHit>>,
    pub(crate) disks: Vec<SimulatedDisk>,
    pub(crate) watchtowers: Option<Vec<Tower>>,
    pub(crate) tower_hits: Vec<Vec<ChainFaultHit>>,
}

struct ChannelSnapshot {
    bytes: Vec<u8>,
    state: Rc<ChannelActorState>,
}

impl Harness {
    pub(crate) fn new(
        seed: u64,
        path: &std::path::Path,
        trace: Trace,
        edges: &[(usize, usize)],
        duration: Duration,
        tcp_capacity: Option<usize>,
        tower_mode: Option<TowerMode>,
    ) -> Result<Self> {
        let mut builder = turmoil::Builder::new();
        builder
            .rng_seed(seed)
            .epoch(UNIX_EPOCH + EPOCH)
            .tick_duration(TICK)
            .min_message_latency(Duration::from_millis(1))
            .max_message_latency(Duration::from_millis(4))
            .simulation_duration(duration);
        if let Some(capacity) = tcp_capacity {
            builder.tcp_capacity(capacity);
        }
        let mut world = builder.build();
        let count = edges
            .iter()
            .map(|(a, b)| (*a).max(*b))
            .max()
            .expect("graph edges")
            + 1;
        let nodes = Rc::new(RefCell::new((0..count).map(|_| Slot::default()).collect()));
        let chain = SimulatedChain::default();
        let mut identities = StdRng::seed_from_u64(seed ^ 0x6e6f_6465);
        let keys: Vec<_> = (1..=count)
            .map(|id| Privkey::from([id as u8; 32]))
            .collect();
        let peers: Vec<_> = keys.iter().map(Privkey::pubkey).collect();
        let links: BTreeMap<_, _> = edges
            .iter()
            .flat_map(|&(a, b)| [(a, b), (b, a)])
            .map(|edge| (edge, LinkControl::default()))
            .collect();
        let chain_access: Vec<_> = (0..count)
            .map(|_| SimulatedChainAccess::default())
            .collect();
        let watchtowers = tower_mode.map(|mode| {
            (0..count)
                .map(|node| Tower::new(node, mode, trace.clone()))
                .collect::<Vec<_>>()
        });
        let mut disks = Vec::with_capacity(count);
        for (index, key) in keys.into_iter().enumerate() {
            let path = path.join(NAMES[index]);
            let disk = SimulatedDisk::new(&path).map_err(|error| anyhow!(error))?;
            disks.push(disk.clone());
            let host = Host {
                index,
                key,
                peers: links
                    .iter()
                    .filter(|((from, _), _)| *from == index)
                    .map(|(&(_, to), control)| Peer {
                        index: to,
                        key: peers[to],
                        control: control.clone(),
                    })
                    .collect(),
                entropy: identities.random(),
                path,
                chain: chain.clone(),
                access: chain_access[index].clone(),
                disk,
                nodes: nodes.clone(),
                trace: trace.clone(),
                watchtower: watchtowers.as_ref().map(|controls| controls[index].clone()),
            };
            world.host(NAMES[index], move || {
                let host = host.clone();
                async move { Ok(host.run().await?) }
            });
        }
        if let Some(towers) = watchtowers
            .as_ref()
            .filter(|_| tower_mode == Some(TowerMode::External))
        {
            for (node, tower) in towers.iter().enumerate() {
                let tower = tower.clone();
                let store_path = path.join(tower.name());
                let chain = chain.clone();
                let key = Privkey::from([(node + 1) as u8; 32]);
                world.host(tower.name(), move || {
                    let tower = tower.clone();
                    let path = store_path.clone();
                    let chain = chain.clone();
                    let key = key.clone();
                    async move { Ok(tower.run_external(path, chain, key).await?) }
                });
            }
        }
        Ok(Self {
            world,
            nodes,
            peers,
            trace,
            snapshots: RefCell::default(),
            links,
            chain,
            chain_access,
            chain_hits: vec![Vec::new(); count],
            disks,
            watchtowers,
            tower_hits: vec![Vec::new(); count],
        })
    }

    pub(crate) fn note(&self, event: impl std::fmt::Display) {
        let ms = self.world.elapsed().as_millis();
        self.trace
            .borrow_mut()
            .push(format!("driver +{ms:05}ms {event}"));
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.world.elapsed()
    }

    pub(crate) fn step(&mut self) -> Result<()> {
        self.world
            .step()
            .map_err(|error| anyhow!(error.to_string()))?;
        for (node, (access, name)) in self.chain_access.iter().zip(NAMES).enumerate() {
            for hit in access.take_hits() {
                self.note(format_args!(
                    "{name} CKB {:?} {} {:?}",
                    hit.endpoint, hit.request, hit.fault
                ));
                self.chain_hits[node].push(hit);
            }
        }
        if let Some(towers) = &self.watchtowers {
            for (node, tower) in towers.iter().enumerate() {
                for hit in tower.access.take_hits() {
                    self.note(format_args!(
                        "{} CKB {:?} {} {:?}",
                        tower.name(),
                        hit.endpoint,
                        hit.request,
                        hit.fault
                    ));
                    self.tower_hits[node].push(hit);
                }
            }
        }
        // Deliver real signed advertisements as a stable batch to each graph.
        let mut nodes = self.nodes.borrow_mut();
        let broadcasts: Vec<_> = nodes
            .iter_mut()
            .flat_map(|slot| std::mem::take(&mut slot.broadcasts))
            .collect();
        if !broadcasts.is_empty() {
            for slot in nodes.iter().filter(|slot| !slot.storage_failed) {
                if let Some(node) = &slot.node {
                    send(
                        &node.actor,
                        NetworkActorMessage::new_event(NetworkActorEvent::GossipMessageUpdates(
                            GossipMessageUpdates::new(broadcasts.clone()),
                        )),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn link(&self, a: usize, b: usize) -> bool {
        let nodes = self.nodes.borrow();
        nodes[a].connected.contains(&b) && nodes[b].connected.contains(&a)
    }

    pub(crate) fn node_available(&self, node: usize) -> bool {
        let nodes = self.nodes.borrow();
        nodes[node].node.is_some()
            && !nodes[node].storage_failed
            && self.disks[node].hit().is_none()
    }

    pub(crate) fn request<T>(
        &self,
        node: usize,
        command: impl FnOnce(RpcReplyPort<T>) -> NetworkActorCommand,
    ) -> Result<oneshot::Receiver<T>> {
        let (reply, receiver) = oneshot::channel();
        let nodes = self.nodes.borrow();
        let node = nodes[node].node.as_ref().context("node is down")?;
        send(
            &node.actor,
            NetworkActorMessage::new_command(command(reply.into())),
        )?;
        Ok(receiver)
    }

    pub(crate) fn funding_aborted(&self, node: usize, channel: Hash256) -> bool {
        self.nodes.borrow()[node]
            .aborted_channels
            .contains(&channel)
    }

    pub(crate) fn funding_record_absent(&self, node: usize, channel: Hash256) -> Result<bool> {
        ensure!(
            self.node_available(node),
            "{} funding store unavailable",
            NAMES[node]
        );
        let nodes = self.nodes.borrow();
        let store = &nodes[node]
            .node
            .as_ref()
            .context("funding store offline")?
            .store;
        Ok(simulation::channel_bytes(store, &channel).is_none())
    }

    pub(crate) fn channel(&self, node: usize, channel: &Hash256) -> Option<Rc<ChannelActorState>> {
        let nodes = self.nodes.borrow();
        if nodes[node].storage_failed || self.disks[node].hit().is_some() {
            return None;
        }
        let store = &nodes[node].node.as_ref()?.store;
        let bytes = simulation::channel_bytes(store, channel)?;
        let mut snapshots = self.snapshots.borrow_mut();
        let key = (node, *channel);
        if let Some(previous) = snapshots
            .get(&key)
            .filter(|previous| previous.bytes == bytes)
        {
            return Some(previous.state.clone());
        }
        // V2 deserialization verifies persisted signing state. Only repeat that work
        // when SQLite shows a new record.
        let state = Rc::new(store.get_channel_actor_state(channel)?);
        snapshots.insert(
            key,
            ChannelSnapshot {
                bytes,
                state: state.clone(),
            },
        );
        Some(state)
    }

    pub(crate) fn channel_online(&self, a: usize, b: usize, channel: &Hash256) -> bool {
        self.link(a, b)
            && [a, b].into_iter().all(|node| {
                self.channel(node, channel).is_some_and(|state| {
                    state.state == ChannelState::ChannelReady
                        && state.connectivity_state == ChannelConnectivityState::Online
                })
            })
    }

    pub(crate) fn payment_status(&self, from: usize, hash: Hash256) -> Option<PaymentStatus> {
        let nodes = self.nodes.borrow();
        if nodes[from].storage_failed || self.disks[from].hit().is_some() {
            return None;
        }
        let node = nodes[from].node.as_ref()?;
        node.store
            .get_payment_session(hash)
            .map(|session| session.effective_status(&node.store))
    }

    pub(crate) fn crash(&mut self, node: usize) {
        self.note(format_args!("crash {}", NAMES[node]));
        self.snapshots
            .get_mut()
            .retain(|(owner, _), _| *owner != node);
        let connected = {
            let mut nodes = self.nodes.borrow_mut();
            let slot = &mut nodes[node];
            slot.node = None;
            slot.settlement_observer = None;
            slot.pending_channels.clear();
            slot.broadcasts.clear();
            slot.storage_failed = false;
            std::mem::take(&mut slot.connected)
        };
        if let Some(towers) = &self.watchtowers {
            let tower = &towers[node];
            if tower.mode == TowerMode::BuiltIn {
                tower.actor.take();
                tower.store.take();
            }
        }
        self.world.crash(NAMES[node]);
        // A dialer can crash before Turmoil creates its TcpStream, leaving the
        // accepted socket on the surviving host without a FIN. Retire unfinished
        // connections explicitly; established sockets still close through TCP.
        for (&(local, remote), control) in &self.links {
            if remote == node && !connected.contains(&local) {
                control.reset();
            }
        }
    }

    pub(crate) fn restart(&mut self, node: usize) {
        self.note(format_args!("restart {}", NAMES[node]));
        self.world.bounce(NAMES[node]);
    }

    pub(crate) fn set_tower_partition(&self, node: usize, partition: bool) -> Result<()> {
        let tower = &self.watchtowers.as_ref().context("watchtower controls")?[node];
        ensure!(
            tower.mode == TowerMode::External,
            "network partition requires an external tower"
        );
        let owner = NAMES[node];
        let name = tower.name();
        if partition {
            self.world.partition(owner, name);
            self.note(format_args!(
                "partition {owner}<->{name} external watchtower RPC"
            ));
        } else {
            self.world.repair(owner, name);
            self.note(format_args!(
                "repair {owner}<->{name} external watchtower RPC"
            ));
        }
        Ok(())
    }

    pub(crate) fn reset_link(&self, a: usize, b: usize) {
        self.links[&(a, b)].reset();
        self.links[&(b, a)].reset();
    }

    pub(crate) fn partition(&mut self, from: usize, to: usize, one_way: bool) {
        if one_way {
            self.world.partition_oneway(NAMES[from], NAMES[to]);
        } else {
            self.world.partition(NAMES[from], NAMES[to]);
        }
    }

    pub(crate) fn repair(&mut self, from: usize, to: usize, one_way: bool) {
        if one_way {
            self.world.repair_oneway(NAMES[from], NAMES[to]);
        } else {
            self.world.repair(NAMES[from], NAMES[to]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{simulation, Duration, Harness, Trace};

    #[test]
    fn listener_reaccepts_after_dialer_crashes_before_connect_returns() {
        simulation::run(
            34,
            || turmoil::since_epoch().map(|time| time.as_millis() as u64),
            || {
                let directory = tempfile::tempdir().unwrap();
                let mut harness = Harness::new(
                    34,
                    directory.path(),
                    Trace::default(),
                    &[(0, 1)],
                    Duration::from_secs(1),
                    None,
                    None,
                )
                .unwrap();
                for _ in 0..10 {
                    harness.step().unwrap();
                    if harness.nodes.borrow()[1].connected.contains(&0) {
                        break;
                    }
                }
                assert!(harness.nodes.borrow()[1].connected.contains(&0));
                assert!(!harness.link(0, 1));

                harness.crash(0);
                for _ in 0..5 {
                    harness.step().unwrap();
                }
                assert!(!harness.nodes.borrow()[1].connected.contains(&0));

                harness.restart(0);
                for _ in 0..40 {
                    harness.step().unwrap();
                    if harness.link(0, 1) {
                        break;
                    }
                }
                assert!(harness.link(0, 1));
            },
        );
    }
}
