use std::cell::{Cell, RefCell};
use std::collections::{hash_map::DefaultHasher, BTreeSet, VecDeque};
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
use fnn::simulation::SimulatedPeerSender;
use fnn::store::{open_store_with_migration, Store};
use fnn::watchtower::WatchtowerMessage;
use ractor::ActorRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use turmoil::net::{TcpListener, TcpStream};

use crate::chain::SimulatedChain;
use crate::chain_fault::SimulatedChainAccess;
use crate::lsp::{serve_lsp_rpc, LspHandle, LspHostConfig};
use crate::lsp_runtime::start_lsp_runtime;
use crate::sqlite_vfs::SimulatedDisk;
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

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameKindFilter {
    Any,
    OpenChannelV2,
    AcceptChannelV2,
    TxUpdate,
    TxSignatures,
    TxCompleteV2,
    TxAbort,
    AddTlc,
    RemoveTlc,
    CommitmentSignedV2,
    RevokeAndAckV2,
    ChannelReadyV2,
    ReestablishChannelV2,
    ShutdownV2,
    ClosingSignedV2,
    AnnouncementSignatures,
    UpdateTlcInfo,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameMatcher {
    pub kind: FrameKindFilter,
    pub channel_id: Option<Hash256>,
}

#[allow(dead_code)]
impl FrameMatcher {
    pub fn any() -> Self {
        Self {
            kind: FrameKindFilter::Any,
            channel_id: None,
        }
    }

    pub fn kind(kind: FrameKindFilter) -> Self {
        Self {
            kind,
            channel_id: None,
        }
    }

    pub fn channel(kind: FrameKindFilter, channel_id: Hash256) -> Self {
        Self {
            kind,
            channel_id: Some(channel_id),
        }
    }

    pub fn matches(&self, message: &FiberMessage) -> bool {
        let kind_matches = match self.kind {
            FrameKindFilter::Any => true,
            FrameKindFilter::OpenChannelV2 => {
                matches!(message, FiberMessage::ChannelInitializationV2(_))
            }
            FrameKindFilter::AcceptChannelV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AcceptChannelV2(_))
            ),
            FrameKindFilter::TxUpdate => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxUpdate(_))
            ),
            FrameKindFilter::TxSignatures => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(_))
            ),
            FrameKindFilter::TxCompleteV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxCompleteV2(_))
            ),
            FrameKindFilter::TxAbort => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxAbort(_))
            ),
            FrameKindFilter::AddTlc => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_))
            ),
            FrameKindFilter::RemoveTlc => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(_))
            ),
            FrameKindFilter::CommitmentSignedV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(_))
            ),
            FrameKindFilter::RevokeAndAckV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(_))
            ),
            FrameKindFilter::ChannelReadyV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(_))
            ),
            FrameKindFilter::ReestablishChannelV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannelV2(_))
            ),
            FrameKindFilter::ShutdownV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ShutdownV2(_))
            ),
            FrameKindFilter::ClosingSignedV2 => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ClosingSignedV2(_))
            ),
            FrameKindFilter::AnnouncementSignatures => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AnnouncementSignatures(
                    _
                ))
            ),
            FrameKindFilter::UpdateTlcInfo => matches!(
                message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::UpdateTlcInfo(_))
            ),
        };
        if !kind_matches {
            return false;
        }
        let Some(expected_channel) = self.channel_id else {
            return true;
        };
        match message {
            FiberMessage::ChannelInitializationV2(open) => open.channel_id == expected_channel,
            FiberMessage::ChannelNormalOperation(channel_msg) => {
                channel_msg.get_channel_id() == expected_channel
            }
            FiberMessage::Init(_) => false,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlackholeRule {
    pub matcher: FrameMatcher,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DuplicateRule {
    pub matcher: FrameMatcher,
    pub copies: usize,
    pub remaining: Option<usize>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdversarialDelayRule {
    pub matcher: FrameMatcher,
    pub duration: Duration,
    pub out_of_order: bool,
    pub persistent: bool,
}

#[allow(dead_code, clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub(crate) enum InjectedWireAction {
    SendFrame(FiberMessage),
    SendRawFrame { declared_len: u32, payload: Vec<u8> },
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
    blackhole: Rc<Cell<Option<BlackholeRule>>>,
    blackholed_frames: Rc<Cell<u64>>,
    duplicate: Rc<Cell<Option<DuplicateRule>>>,
    duplicated_frames: Rc<Cell<u64>>,
    adv_delay: Rc<Cell<Option<AdversarialDelayRule>>>,
    adv_delayed_frames: Rc<Cell<u64>>,
    allow_malformed_disconnect: Rc<Cell<bool>>,
    malformed_disconnects: Rc<Cell<u64>>,
    last_captured: Rc<RefCell<Option<FiberMessage>>>,
    injected_queue: Rc<RefCell<VecDeque<InjectedWireAction>>>,
    injected_notify: watch::Sender<u64>,
    injected_sent: Rc<Cell<u64>>,
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

    #[allow(dead_code)]
    pub fn set_blackhole(&self, rule: Option<BlackholeRule>) {
        self.blackhole.set(rule);
    }

    #[allow(dead_code)]
    pub fn blackholed_frames(&self) -> u64 {
        self.blackholed_frames.get()
    }

    #[allow(dead_code)]
    pub fn set_duplicate(&self, rule: Option<DuplicateRule>) {
        self.duplicate.set(rule);
    }

    #[allow(dead_code)]
    pub fn duplicated_frames(&self) -> u64 {
        self.duplicated_frames.get()
    }

    #[allow(dead_code)]
    pub fn set_adv_delay(&self, rule: Option<AdversarialDelayRule>) {
        self.adv_delay.set(rule);
    }

    #[allow(dead_code)]
    pub fn adv_delayed_frames(&self) -> u64 {
        self.adv_delayed_frames.get()
    }

    #[allow(dead_code)]
    pub fn set_allow_malformed_disconnect(&self, allow: bool) {
        self.allow_malformed_disconnect.set(allow);
    }

    #[allow(dead_code)]
    pub fn malformed_disconnects(&self) -> u64 {
        self.malformed_disconnects.get()
    }

    #[allow(dead_code)]
    pub fn last_captured(&self) -> Option<FiberMessage> {
        self.last_captured.borrow().clone()
    }

    #[allow(dead_code)]
    pub fn push_wire_action(&self, action: InjectedWireAction) {
        self.injected_queue.borrow_mut().push_back(action);
        self.injected_notify
            .send_modify(|seq| *seq = seq.wrapping_add(1));
    }

    #[allow(dead_code)]
    pub fn push_wire_actions(&self, actions: impl IntoIterator<Item = InjectedWireAction>) {
        let mut queue = self.injected_queue.borrow_mut();
        let before = queue.len();
        queue.extend(actions);
        if queue.len() > before {
            drop(queue);
            self.injected_notify
                .send_modify(|seq| *seq = seq.wrapping_add(1));
        }
    }

    #[allow(dead_code)]
    pub fn clear_wire_actions(&self) {
        self.injected_queue.borrow_mut().clear();
    }

    #[allow(dead_code)]
    pub fn injected_sent(&self) -> u64 {
        self.injected_sent.get()
    }

    fn pop_wire_action(&self) -> Option<InjectedWireAction> {
        self.injected_queue.borrow_mut().pop_front()
    }

    fn capture_received_frame(&self, message: &FiberMessage) {
        *self.last_captured.borrow_mut() = Some(message.clone());
    }

    fn blackhole_received_frame(&self, message: &FiberMessage) -> bool {
        let hit = self
            .blackhole
            .get()
            .is_some_and(|rule| rule.matcher.matches(message));
        if hit {
            self.blackholed_frames.set(self.blackholed_frames.get() + 1);
        }
        hit
    }

    fn duplicate_copies(&self, message: &FiberMessage) -> usize {
        let Some(mut rule) = self.duplicate.get() else {
            return 1;
        };
        if rule.remaining == Some(0) {
            self.duplicate.set(None);
            return 1;
        }
        if !rule.matcher.matches(message) {
            return 1;
        }
        let copies = rule.copies.max(1);
        if let Some(remaining) = rule.remaining.as_mut() {
            if *remaining <= 1 {
                self.duplicate.set(None);
            } else {
                *remaining -= 1;
                self.duplicate.set(Some(rule));
            }
        }
        if copies > 1 {
            self.duplicated_frames
                .set(self.duplicated_frames.get() + (copies - 1) as u64);
        }
        copies
    }

    fn next_adv_delay(&self, message: &FiberMessage, out_of_order: bool) -> Option<Duration> {
        let rule = self.adv_delay.get()?;
        if rule.out_of_order != out_of_order || !rule.matcher.matches(message) {
            return None;
        }
        if !rule.persistent {
            self.adv_delay.set(None);
        }
        self.adv_delayed_frames
            .set(self.adv_delayed_frames.get() + 1);
        Some(rule.duration)
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
    pub lsp: Option<LspHostConfig>,
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
        if self.lsp.is_some() {
            config.set_simulation_announced_addr("/ip4/127.0.0.1/tcp/8228".to_string(), 1);
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
        let mut runtime = self
            .chain
            .start_node(
                config.clone(),
                self.key.clone(),
                entropy,
                store.clone(),
                self.access.clone(),
            )
            .await?;
        {
            let mut nodes = self.nodes.borrow_mut();
            let slot = &mut nodes[self.index];
            slot.storage_failed = false;
            slot.node = Some(Node {
                actor: runtime.actor.clone(),
                store: store.clone(),
            });
        }
        record(
            &self.trace,
            format_args!("{} start {generation}", NAMES[self.index]),
        );

        let mut tasks = tokio::task::JoinSet::new();
        let _lsp_runtime = if let Some(lsp_cfg) = &self.lsp {
            let lsp_runtime = Rc::new(
                start_lsp_runtime(
                    &runtime,
                    config.clone(),
                    self.key.clone(),
                    store.clone(),
                    lsp_cfg.dir.clone(),
                    lsp_cfg.biscuit_seed,
                    lsp_cfg.config.clone(),
                )
                .await?,
            );
            lsp_cfg.slot.replace(Some(LspHandle {
                actor: lsp_runtime.lsp_actor.clone(),
                admin_token: lsp_runtime.admin_token.clone(),
                _runtime: lsp_runtime.clone(),
            }));
            let rpc_runtime = lsp_runtime.clone();
            let rpc_disk = self.disk.clone();
            tasks.spawn_local(async move { serve_lsp_rpc(rpc_runtime, rpc_disk).await });
            record(
                &self.trace,
                format_args!("{} LSP service started", NAMES[self.index]),
            );
            Some(lsp_runtime)
        } else {
            None
        };
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
                        let disconnected = {
                            let mut nodes = self.nodes.borrow_mut();
                            let slot = &mut nodes[self.index];
                            slot.storage_failed = true;
                            std::mem::take(&mut slot.connected)
                        };
                        for peer in disconnected {
                            record(
                                &self.trace,
                                format_args!("{} disconnected from {}", NAMES[self.index], NAMES[peer]),
                            );
                        }
                        if let Some(lsp_cfg) = &self.lsp {
                            lsp_cfg.slot.replace(None);
                        }
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
                        NetworkServiceEvent::ChannelPendingToBeAccepted(peer, id) => {
                            self.nodes.borrow_mut()[self.index].pending_channels.insert(id);
                            if let Some(lsp_cfg) = &self.lsp {
                                if self.peers.iter().all(|known| known.key != peer) {
                                    let (reply, _rx) = oneshot::channel();
                                    let _ = send(
                                        &runtime.actor,
                                        NetworkActorMessage::new_command(
                                            FiberActorCommand::AcceptChannel(
                                                fnn::fiber::network::AcceptChannelCommand {
                                                    temp_channel_id: id,
                                                    funding_amount: lsp_cfg.tenant_liquidity,
                                                    shutdown_script: None,
                                                    max_tlc_value_in_flight: None,
                                                    max_tlc_number_in_flight: None,
                                                    min_tlc_value: None,
                                                    tlc_fee_proportional_millionths: Some(0),
                                                    tlc_expiry_delta: None,
                                                },
                                                reply.into(),
                                            ),
                                        ),
                                    );
                                }
                            }
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

            if self.disk.hit().is_some() {
                return std::future::pending().await;
            }
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
            let mut deferred = tokio::task::JoinSet::new();
            loop {
                let len = tokio::select! {
                    biased;
                    Some(result) = deferred.join_next(), if !deferred.is_empty() => {
                        result.context("deferred peer frame task failed")??;
                        continue;
                    }
                    len = reader.read_u32() => len? as usize,
                };
                if len > MAX_FRAME_SIZE {
                    if peer.control.allow_malformed_disconnect.get() {
                        peer.control
                            .malformed_disconnects
                            .set(peer.control.malformed_disconnects.get() + 1);
                        record(
                            &self.trace,
                            format_args!(
                                "{} disconnected {} on oversized Fiber frame len={len}",
                                NAMES[self.index], NAMES[peer.index]
                            ),
                        );
                        return Ok(());
                    }
                    bail!("oversized Fiber frame");
                }
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).await?;
                peer.control.fail(TcpFault::Read)?;
                let message = match FiberMessage::from_molecule_slice(&bytes) {
                    Ok(message) => message,
                    Err(error) => {
                        if peer.control.allow_malformed_disconnect.get() {
                            peer.control
                                .malformed_disconnects
                                .set(peer.control.malformed_disconnects.get() + 1);
                            record(
                                &self.trace,
                                format_args!(
                                    "{} disconnected {} on malformed Fiber frame: {error}",
                                    NAMES[self.index], NAMES[peer.index]
                                ),
                            );
                            return Ok(());
                        }
                        return Err(error.into());
                    }
                };
                peer.control.capture_received_frame(&message);
                let kind = frame_kind(&message);
                let mut digest = DefaultHasher::new();
                bytes.hash(&mut digest);
                let digest = digest.finish();
                if peer.control.blackhole_received_frame(&message) {
                    record(
                        &self.trace,
                        format_args!(
                            "{} blackholed peer frame from {} {kind} {digest:016x}",
                            NAMES[self.index], NAMES[peer.index]
                        ),
                    );
                    continue;
                }
                if peer.control.drop_received_frame(&message) {
                    record(
                        &self.trace,
                        format_args!(
                            "{} dropped peer frame from {} {kind} {digest:016x}",
                            NAMES[self.index], NAMES[peer.index]
                        ),
                    );
                    continue;
                }
                if let Some(delay) = peer.control.next_adv_delay(&message, true) {
                    record(
                        &self.trace,
                        format_args!(
                            "{} deferring out-of-order peer frame from {} {kind} {}ms {digest:016x}",
                            NAMES[self.index],
                            NAMES[peer.index],
                            delay.as_millis()
                        ),
                    );
                    let actor = actor.clone();
                    let peer_key = peer.key;
                    let trace = self.trace.clone();
                    let local_name = NAMES[self.index];
                    let remote_name = NAMES[peer.index];
                    deferred.spawn_local(async move {
                        tokio::time::sleep(delay).await;
                        record(
                            &trace,
                            format_args!(
                                "{local_name} delivering deferred peer frame from {remote_name} {kind} {digest:016x}"
                            ),
                        );
                        send(
                            &actor,
                            NetworkActorMessage::new_event(PublicNetworkEvent::FiberMessage(
                                peer_key, message, None,
                            )),
                        )
                    });
                    continue;
                }
                record(
                    &self.trace,
                    format_args!(
                        "{} receive from {} {kind} {digest:016x}",
                        NAMES[self.index], NAMES[peer.index]
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
            let mut injected = peer.control.injected_notify.subscribe();
            loop {
                paused.wait_for(|paused| !paused).await?;
                injected.borrow_and_update();
                if let Some(action) = peer.control.pop_wire_action() {
                    match action {
                        InjectedWireAction::SendFrame(message) => {
                            let kind = frame_kind(&message);
                            let bytes = message.to_molecule_bytes();
                            anyhow::ensure!(bytes.len() <= MAX_FRAME_SIZE, "oversized Fiber frame");
                            record(
                                &self.trace,
                                format_args!(
                                    "{} injecting peer frame to {} {kind}",
                                    NAMES[self.index], NAMES[peer.index]
                                ),
                            );
                            writer.write_u32(bytes.len() as u32).await?;
                            writer.write_all(&bytes).await?;
                        }
                        InjectedWireAction::SendRawFrame {
                            declared_len,
                            payload,
                        } => {
                            record(
                                &self.trace,
                                format_args!(
                                    "{} injecting raw frame to {} declared_len={declared_len} payload_len={}",
                                    NAMES[self.index],
                                    NAMES[peer.index],
                                    payload.len()
                                ),
                            );
                            writer.write_u32(declared_len).await?;
                            if !payload.is_empty() {
                                writer.write_all(&payload).await?;
                            }
                        }
                    }
                    peer.control
                        .injected_sent
                        .set(peer.control.injected_sent.get() + 1);
                    continue;
                }
                // Observe a newly paused writer or newly queued injected action while waiting for the next frame.
                let message = tokio::select! {
                    biased;
                    result = paused.changed() => {
                        result?;
                        continue;
                    }
                    result = injected.changed() => {
                        result?;
                        continue;
                    }
                    message = outgoing.recv() => message.context("Fiber outbox exited")?,
                };
                if let Some(delay) = peer
                    .control
                    .next_frame_delay()
                    .or_else(|| peer.control.next_adv_delay(&message, false))
                {
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
                let copies = peer.control.duplicate_copies(&message);
                let duplicate_kind = (copies > 1).then(|| frame_kind(&message));
                let bytes = message.to_molecule_bytes();
                anyhow::ensure!(bytes.len() <= MAX_FRAME_SIZE, "oversized Fiber frame");
                if let Some(kind) = duplicate_kind {
                    record(
                        &self.trace,
                        format_args!(
                            "{} duplicating peer frame to {} {kind} copies={copies}",
                            NAMES[self.index], NAMES[peer.index]
                        ),
                    );
                }
                for _ in 0..copies {
                    writer.write_u32(bytes.len() as u32).await?;
                    writer.write_all(&bytes).await?;
                }
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
