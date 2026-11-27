use std::cell::{Cell, RefCell};
use std::collections::{hash_map::DefaultHasher, BTreeSet};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use fnn::fiber::types::{BroadcastMessageWithTimestamp, FiberChannelMessage, FiberMessage};
use fnn::fiber::{
    FeatureVector, FiberActorCommand, FiberConfig, NetworkActorMessage, NetworkServiceEvent,
    Privkey, Pubkey, PublicNetworkEvent,
};
use fnn::fiber_types::Hash256;
use fnn::simulation::{
    self, SimulatedChain, SimulatedChainAccess, SimulatedDisk, SimulatedPeerSender,
};
use fnn::store::{open_store_with_migration, Store};
use fnn::watchtower::WatchtowerMessage;
use ractor::ActorRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use turmoil::net::{TcpListener, TcpStream};

use crate::tower::{self, Tower, TowerMode};

const PORT: u16 = 7100;
const RECONNECT_DELAY: Duration = Duration::from_millis(25);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_FRAME_SIZE: usize = 1 << 20;
const QUEUE_FRAMES: usize = 1;

pub(crate) type Trace = Rc<RefCell<Vec<String>>>;
pub(crate) type Nodes = Rc<RefCell<Vec<Slot>>>;

#[derive(Default)]
pub(crate) struct Slot {
    pub generation: u64,
    pub node: Option<Node>,
    pub settlement_observer: Option<ActorRef<WatchtowerMessage>>,
    pub connected: BTreeSet<usize>,
    pub pending_channels: BTreeSet<Hash256>,
    pub aborted_channels: BTreeSet<Hash256>,
    pub storage_failed: bool,
    pub broadcasts: Vec<BroadcastMessageWithTimestamp>,
}

pub(crate) struct Node {
    pub actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
}

pub(crate) const NAMES: [&str; 8] = ["A", "B", "C", "D", "E", "F", "G", "H"];

pub(crate) fn record(trace: &Trace, message: impl std::fmt::Display) {
    let ms = turmoil::sim_elapsed().unwrap_or_default().as_millis();
    trace
        .borrow_mut()
        .push(format!("host +{ms:05}ms {message}"));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TcpFault {
    Dial,
    Read,
    Write,
}

#[derive(Clone, Copy)]
pub(crate) enum FrameLoss {
    After(usize),
    FundingUpdate(Hash256),
    FundingSignatures(Hash256),
    RevokeAndAck(Hash256),
}

#[derive(Clone, Copy)]
pub(crate) struct FrameDelay {
    pub after: usize,
    pub duration: Duration,
}

#[derive(Clone, Default)]
pub(crate) struct LinkControl {
    reset: watch::Sender<()>,
    paused: watch::Sender<bool>,
    fault: Rc<RefCell<Option<TcpFault>>>,
    hits: Rc<RefCell<Vec<TcpFault>>>,
    blocked: Arc<AtomicU64>,
    frame_loss: Rc<Cell<Option<FrameLoss>>>,
    dropped_frames: Rc<Cell<u64>>,
    frame_delay: Rc<Cell<Option<FrameDelay>>>,
    delayed_frames: Rc<Cell<u64>>,
}

impl LinkControl {
    pub fn reset(&self) {
        self.reset.send_modify(|_| ());
    }

    pub fn pause_writer(&self, pause: bool) {
        self.paused.send_replace(pause);
    }

    pub fn blocked(&self) -> u64 {
        self.blocked.load(Ordering::Relaxed)
    }

    pub fn set_frame_loss(&self, rule: Option<FrameLoss>) {
        self.frame_loss.set(rule);
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.get()
    }

    pub fn set_frame_delay(&self, rule: Option<FrameDelay>) {
        self.frame_delay.set(rule);
    }

    pub fn delayed_frames(&self) -> u64 {
        self.delayed_frames.get()
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        let mut rule = self.frame_delay.get()?;
        if rule.after != 0 {
            rule.after -= 1;
            self.frame_delay.set(Some(rule));
            return None;
        }
        self.frame_delay.set(None);
        self.delayed_frames.set(self.delayed_frames.get() + 1);
        Some(rule.duration)
    }

    fn drop_received_frame(&self, message: &FiberMessage) -> bool {
        let hit = match self.frame_loss.get() {
            Some(FrameLoss::After(0)) => true,
            Some(FrameLoss::After(remaining)) => {
                self.frame_loss.set(Some(FrameLoss::After(remaining - 1)));
                false
            }
            Some(FrameLoss::FundingUpdate(channel)) => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxUpdate(update))
                    if update.channel_id == channel
            ),
            Some(FrameLoss::FundingSignatures(channel)) => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(signatures))
                    if signatures.channel_id == channel
            ),
            Some(FrameLoss::RevokeAndAck(channel)) => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(ack))
                    if ack.channel_id == channel
            ),
            None => false,
        };
        if hit {
            self.frame_loss.set(None);
            self.dropped_frames.set(self.dropped_frames.get() + 1);
        }
        hit
    }

    pub fn arm(&self, fault: TcpFault) {
        self.hits.borrow_mut().clear();
        self.fault.replace(Some(fault));
    }

    pub fn disarm(&self) {
        self.fault.take();
    }

    pub fn hit(&self, fault: TcpFault) -> bool {
        self.hits.borrow().contains(&fault)
    }

    fn fail(&self, operation: TcpFault) -> io::Result<()> {
        if *self.fault.borrow() != Some(operation) {
            return Ok(());
        }
        self.fault.take();
        self.hits.borrow_mut().push(operation);
        let kind = if operation == TcpFault::Dial {
            io::ErrorKind::ConnectionRefused
        } else {
            io::ErrorKind::ConnectionReset
        };
        Err(io::Error::new(kind, format!("simulated TCP {operation:?}")))
    }
}

#[derive(Clone)]
pub(crate) struct Peer {
    pub index: usize,
    pub key: Pubkey,
    pub control: LinkControl,
}

#[derive(Clone)]
pub(crate) struct Host {
    pub index: usize,
    pub key: Privkey,
    pub peers: Vec<Peer>,
    pub entropy: [u8; 32],
    pub path: PathBuf,
    pub chain: SimulatedChain,
    pub access: SimulatedChainAccess,
    pub disk: SimulatedDisk,
    pub nodes: Nodes,
    pub trace: Trace,
    pub watchtower: Option<Tower>,
    pub funding_timeout_seconds: Option<u64>,
}

impl Host {
    pub async fn run(self) -> Result<()> {
        let store = open_store_with_migration(
            &self.path,
            true,
            fnn::fiber::config::DEFAULT_STORE_CHANNEL_CACHE_CAPACITY,
            Box::new(|_| true),
            Box::new(|_| {}),
        )
        .map_err(|error| anyhow!(error))?;
        let mut config = FiberConfig::default();
        config.auto_accept_channel_ckb_funding_amount = Some(0);
        if let Some(timeout) = self.funding_timeout_seconds {
            config.funding_timeout_seconds = timeout;
        }
        let features = config.gen_node_features();
        let generation = {
            let mut nodes = self.nodes.borrow_mut();
            nodes[self.index].generation += 1;
            nodes[self.index].generation
        };
        let mut entropy = self.entropy;
        for (byte, generation_byte) in entropy.iter_mut().zip(generation.to_le_bytes()) {
            *byte ^= generation_byte;
        }
        let mut runtime = simulation::start_node(
            config,
            self.key.clone(),
            entropy,
            store.clone(),
            self.chain.clone(),
            self.access.clone(),
        )
        .await?;
        self.nodes.borrow_mut()[self.index].node = Some(Node {
            actor: runtime.actor.clone(),
            store: store.clone(),
        });
        record(
            &self.trace,
            format_args!("{} start {generation}", NAMES[self.index]),
        );

        let mut tasks = tokio::task::JoinSet::new();
        let forward = match &self.watchtower {
            Some(tower) if tower.mode == TowerMode::BuiltIn => {
                let (tower, store, chain, key) = (
                    tower.clone(),
                    store.clone(),
                    self.chain.clone(),
                    self.key.clone(),
                );
                tasks.spawn_local(async move { tower.run(store, chain, key).await });
                None
            }
            Some(tower) => {
                let (sender, receiver) = mpsc::unbounded_channel();
                let (observer, task) = self
                    .chain
                    .start_watchtower(store.clone(), self.key.clone(), self.access.clone(), false)
                    .await?;
                self.nodes.borrow_mut()[self.index].settlement_observer = Some(observer);
                let tower = tower.clone();
                tasks.spawn_local(async move { tower.forward(receiver).await });
                tasks.spawn_local(async move {
                    task.await.context("settlement observer task failed")?;
                    bail!("settlement observer exited");
                });
                Some(sender)
            }
            None => None,
        };

        for peer in &self.peers {
            let host = self.clone();
            let actor = runtime.actor.clone();
            let features = features.clone();
            let peer = peer.clone();
            tasks.spawn_local(async move { host.connection(&actor, features, peer).await });
        }
        loop {
            tokio::select! {
                biased;
                result = &mut runtime.task => {
                    result.context("Fiber actor task failed")?;
                    if let Some(fault) = self.disk.hit() {
                        self.nodes.borrow_mut()[self.index].storage_failed = true;
                        record(&self.trace, format_args!("{} SQLite fault {fault:?} stopped Fiber", NAMES[self.index]));
                        drop(tasks);
                        return std::future::pending().await;
                    }
                    bail!("{} Fiber actor exited", NAMES[self.index]);
                }
                Some(result) = tasks.join_next() => {
                    result.context("Fiber host child task failed")??;
                    bail!("{} Fiber host child task exited", NAMES[self.index]);
                }
                Some(broadcast) = runtime.broadcasts.recv() => {
                    self.nodes.borrow_mut()[self.index].broadcasts.extend(broadcast);
                }
                notification = runtime.events.recv() => {
                    let event = notification.context("Fiber events closed")?;
                    if self.watchtower.is_some() {
                        tower::persist_event(&event, &store);
                    }
                    if let Some(forward) = &forward {
                        forward.send(event.clone()).map_err(|_| anyhow!("external watchtower forwarder stopped"))?;
                    }
                    match event {
                        NetworkServiceEvent::ChannelPendingToBeAccepted(_, id) => {
                            self.nodes.borrow_mut()[self.index].pending_channels.insert(id);
                        }
                        NetworkServiceEvent::ChannelFundingAborted(id) => {
                            self.nodes.borrow_mut()[self.index].aborted_channels.insert(id);
                            record(
                                &self.trace,
                                format_args!("{} funding aborted {id}", NAMES[self.index]),
                            );
                        }
                        _ => {}
                    }
                }

            }
        }
    }

    async fn connection(
        &self,
        actor: &ActorRef<NetworkActorMessage>,
        features: FeatureVector,
        peer: Peer,
    ) -> Result<()> {
        let port = PORT + self.index.min(peer.index) as u16;
        let listener = if self.index > peer.index {
            Some(TcpListener::bind(("0.0.0.0", port)).await?)
        } else {
            None
        };
        let mut reset = peer.control.reset.subscribe();
        loop {
            let socket = if let Some(listener) = &listener {
                listener.accept().await?.0
            } else {
                let dial = async {
                    peer.control.fail(TcpFault::Dial)?;
                    TcpStream::connect((NAMES[peer.index], port)).await
                };
                match tokio::time::timeout(CONNECT_TIMEOUT, dial).await {
                    Ok(Ok(socket)) => socket,
                    _ => {
                        tokio::time::sleep(RECONNECT_DELAY).await;
                        continue;
                    }
                }
            };
            reset.borrow_and_update();
            let (outbox, outgoing) = mpsc::channel(QUEUE_FRAMES);
            let (activate_reader, reader_gate) = oneshot::channel();
            let setup = async {
                ractor::call!(actor, |reply| NetworkActorMessage::new_command(
                    FiberActorCommand::RegisterSimulatedPeer {
                        pubkey: peer.key,
                        sender: SimulatedPeerSender::new(outbox, peer.control.blocked.clone()),
                        features: features.clone(),
                        reply,
                    }
                ))
                .map_err(|error| anyhow!(error.to_string()))?
                .map_err(|error| anyhow!(error))?;
                ractor::call!(actor, |reply| NetworkActorMessage::new_command(
                    FiberActorCommand::ActivateSimulatedPeer(peer.key, reply)
                ))
                .map_err(|error| anyhow!(error.to_string()))?
                .map_err(|error| anyhow!(error))
            };
            let wire = self.wire(&peer, socket, actor, outgoing, reader_gate);
            let result = drive_connection(setup, wire, &mut reset, || {
                self.nodes.borrow_mut()[self.index]
                    .connected
                    .insert(peer.index);
                record(
                    &self.trace,
                    format_args!("{} connected to {}", NAMES[self.index], NAMES[peer.index]),
                );
                let _ = activate_reader.send(());
            })
            .await;

            if self.nodes.borrow_mut()[self.index]
                .connected
                .remove(&peer.index)
            {
                record(
                    &self.trace,
                    format_args!(
                        "{} disconnected from {}",
                        NAMES[self.index], NAMES[peer.index]
                    ),
                );
            }
            // The old wire and its queue are already dropped, so an actor waiting
            // to send can process this unregister before the next registration.
            send(
                actor,
                NetworkActorMessage::new_command(FiberActorCommand::UnregisterSimulatedPeer(
                    peer.key,
                )),
            )?;
            result?;
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    async fn wire(
        &self,
        peer: &Peer,
        socket: TcpStream,
        actor: &ActorRef<NetworkActorMessage>,
        mut outgoing: mpsc::Receiver<FiberMessage>,
        reader_gate: oneshot::Receiver<()>,
    ) -> Result<()> {
        let (mut reader, mut writer) = socket.into_split();
        let read = async {
            reader_gate.await?;
            loop {
                let len = reader.read_u32().await? as usize;
                anyhow::ensure!(len <= MAX_FRAME_SIZE, "oversized Fiber frame");
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).await?;
                peer.control.fail(TcpFault::Read)?;
                let message = FiberMessage::from_molecule_slice(&bytes)?;
                let kind = frame_kind(&message);
                let mut digest = DefaultHasher::new();
                bytes.hash(&mut digest);
                if peer.control.drop_received_frame(&message) {
                    record(
                        &self.trace,
                        format_args!(
                            "{} dropped peer frame from {} {kind} {:016x}",
                            NAMES[self.index],
                            NAMES[peer.index],
                            digest.finish()
                        ),
                    );
                    continue;
                }
                record(
                    &self.trace,
                    format_args!(
                        "{} receive from {} {kind} {:016x}",
                        NAMES[self.index],
                        NAMES[peer.index],
                        digest.finish()
                    ),
                );
                send(
                    actor,
                    NetworkActorMessage::new_event(PublicNetworkEvent::FiberMessage(
                        peer.key, message, None,
                    )),
                )?;
            }
        };
        let write = async {
            let mut paused = peer.control.paused.subscribe();
            loop {
                paused.wait_for(|paused| !paused).await?;
                // Observe a newly paused writer while waiting for the next frame.
                let message = tokio::select! {
                    biased;
                    result = paused.changed() => {
                        result?;
                        continue;
                    }
                    message = outgoing.recv() => message.context("Fiber outbox exited")?,
                };
                if let Some(delay) = peer.control.next_frame_delay() {
                    let kind = frame_kind(&message);
                    record(
                        &self.trace,
                        format_args!(
                            "{} delaying peer frame to {} {kind} {}ms",
                            NAMES[self.index],
                            NAMES[peer.index],
                            delay.as_millis()
                        ),
                    );
                    // A TCP stream cannot deliver the later frames ahead of this one.
                    tokio::time::sleep(delay).await;
                }
                peer.control.fail(TcpFault::Write)?;
                let bytes = message.to_molecule_bytes();
                anyhow::ensure!(bytes.len() <= MAX_FRAME_SIZE, "oversized Fiber frame");
                writer.write_u32(bytes.len() as u32).await?;
                writer.write_all(&bytes).await?;
            }
        };
        let result: Result<()> = tokio::select! {
            biased;
            result = read => result,
            result = write => result,
        };
        match result {
            Err(error)
                if error.downcast_ref::<io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    )
                }) =>
            {
                Ok(())
            }
            result => result,
        }
    }
}

fn frame_kind(message: &FiberMessage) -> String {
    match message {
        FiberMessage::Init(_) => "Init".to_owned(),
        FiberMessage::ChannelInitializationV2(_) => "OpenChannelV2".to_owned(),
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
            format!(
                "AddTlc channel={} TLC={} payment={} amount={}",
                add.channel_id, add.tlc_id, add.payment_hash, add.amount
            )
        }
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxUpdate(update)) => {
            format!("TxUpdate channel={}", update.channel_id)
        }
        FiberMessage::ChannelNormalOperation(message) => message.to_string(),
    }
}

async fn drive_connection(
    setup: impl Future<Output = Result<()>>,
    wire: impl Future<Output = Result<()>>,
    reset: &mut watch::Receiver<()>,
    on_activated: impl FnOnce(),
) -> Result<()> {
    tokio::pin!(wire);
    // Once registered, the actor can fill the queue before it handles Activate.
    // Poll the writer while setup waits for that actor's reply.
    tokio::select! {
        biased;
        _ = reset.changed() => return Ok(()),
        result = setup => result?,
        result = &mut wire => return result,
    }
    on_activated();
    tokio::select! {
        biased;
        _ = reset.changed() => Ok(()),
        result = &mut wire => result,
    }
}

pub(crate) fn send<M: ractor::Message>(actor: &ActorRef<M>, message: M) -> Result<()> {
    actor
        .send_message(message)
        .map_err(|error| anyhow!(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{drive_connection, Duration, QUEUE_FRAMES};
    use tokio::sync::{mpsc, oneshot, watch};

    #[tokio::test(start_paused = true)]
    async fn full_outbox_drains_while_activation_is_pending() {
        let (_reset, mut resets) = watch::channel(());
        let (outbox, mut outgoing) = mpsc::channel(QUEUE_FRAMES);
        let (activated, reader_gate) = oneshot::channel();
        assert_eq!(QUEUE_FRAMES, 1);
        outbox.try_send(1).expect("fill outbox before activation");

        let setup = async {
            // The network actor cannot reply to Activate until this send completes.
            outbox.send(2).await?;
            Ok(())
        };
        let wire = async {
            assert_eq!(outgoing.recv().await, Some(1));
            assert_eq!(outgoing.recv().await, Some(2));
            reader_gate.await?;
            Ok(())
        };
        tokio::time::timeout(
            Duration::from_millis(100),
            drive_connection(setup, wire, &mut resets, || {
                activated.send(()).expect("open the reader gate");
            }),
        )
        .await
        .expect("the full outbox must not deadlock activation")
        .expect("connection completes");
    }
}
