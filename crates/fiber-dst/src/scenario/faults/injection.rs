use std::{cell::RefCell, time::Duration};

use anyhow::{Context, Result};
use fnn::fiber::Hash256;
use fnn::simulation::{
    pause_before_commitment_ack, pause_before_forwarding_fulfill, slow_mailbox, CommitmentPause,
    MailboxDelay,
};
use ractor::ActorCell;

use crate::chain_fault::{ChainEndpoint, ChainFault};
use crate::host::{FrameDelay, FrameLoss, LinkControl, TcpFault};
use crate::sqlite_vfs::{BatchTarget, DiskFault};

use crate::harness::Harness;

pub(in crate::scenario) enum Fault {
    Crash(usize),
    Partition {
        from: usize,
        to: usize,
        one_way: bool,
    },
    Chain {
        node: usize,
        request: Option<(ChainEndpoint, &'static str)>,
        fault: ChainFault,
    },
    Sqlite {
        node: usize,
        fault: DiskFault,
        batch: Option<BatchTarget>,
    },
    Tcp {
        from: usize,
        to: usize,
        fault: TcpFault,
        reconnect: bool,
    },
    DelayPeerFrame {
        from: usize,
        to: usize,
        rule: FrameDelay,
    },
    DropPeerFrame {
        from: usize,
        to: usize,
        rule: FrameLoss,
    },
    Disconnect(usize, usize),
    Writer {
        from: usize,
        to: usize,
    },
    Mailbox {
        target: MailboxTarget,
        delay: Duration,
    },
    ProtocolPause {
        node: usize,
        channel: Hash256,
        point: ProtocolPoint,
        preceding_hits: usize,
    },
}

#[derive(Clone, Copy, Debug)]
pub(in crate::scenario) enum ProtocolPoint {
    CommitmentAck,
    ForwardFulfill,
    FundingPersisted,
    ChannelIdRewritten,
}

pub(in crate::scenario) enum MailboxTarget {
    Network(usize),
    Actor(ActorCell),
}

pub(in crate::scenario) enum Injection {
    Crash(usize),
    Partition {
        from: usize,
        to: usize,
        one_way: bool,
    },
    Chain {
        node: usize,
        first_hit: usize,
        request: Option<(ChainEndpoint, &'static str)>,
        fault: ChainFault,
    },
    Sqlite {
        node: usize,
        fault: DiskFault,
    },
    Tcp {
        control: LinkControl,
        fault: TcpFault,
    },
    DelayPeerFrame {
        control: LinkControl,
        previously_delayed: u64,
    },
    DropPeerFrame {
        control: LinkControl,
        previously_dropped: u64,
        from: usize,
        to: usize,
    },
    Disconnect,
    Writer {
        control: LinkControl,
        previously_blocked: u64,
    },
    Mailbox(MailboxDelay),
    ProtocolPause(RefCell<CommitmentPause>),
}

impl Fault {
    pub(in crate::scenario) fn inject(&self, harness: &mut Harness) -> Result<Injection> {
        Ok(match *self {
            Self::Crash(node) => {
                harness.crash(node);
                Injection::Crash(node)
            }
            Self::Partition { from, to, one_way } => {
                harness.partition(from, to, one_way);
                Injection::Partition { from, to, one_way }
            }
            Self::Chain {
                node,
                request,
                fault,
            } => {
                let access = &harness.chain_access[node];
                let first_hit = harness.chain_hits[node].len();
                match request {
                    Some((endpoint, name)) => access.set_for(endpoint, name, fault),
                    None => access.set(fault),
                }
                Injection::Chain {
                    node,
                    first_hit,
                    request,
                    fault,
                }
            }
            Self::Sqlite {
                node,
                fault,
                ref batch,
            } => {
                match batch {
                    Some(target) => harness.disks[node].arm_batch(fault, target.clone()),
                    None => harness.disks[node].arm(fault),
                }
                Injection::Sqlite { node, fault }
            }
            Self::Tcp {
                from,
                to,
                fault,
                reconnect,
            } => {
                let control = harness.links[&(from, to)].clone();
                control.arm(fault);
                if reconnect {
                    harness.reset_link(from, to);
                }
                Injection::Tcp { control, fault }
            }
            Self::DelayPeerFrame { from, to, rule } => {
                let control = harness.links[&(from, to)].clone();
                let previously_delayed = control.delayed_frames();
                control.set_frame_delay(Some(rule));
                Injection::DelayPeerFrame {
                    control,
                    previously_delayed,
                }
            }
            Self::DropPeerFrame { from, to, rule } => {
                // The receiver owns the read loop for this direction.
                let control = harness.links[&(to, from)].clone();
                let previously_dropped = control.dropped_frames();
                control.set_frame_loss(Some(rule));
                Injection::DropPeerFrame {
                    control,
                    previously_dropped,
                    from,
                    to,
                }
            }
            Self::Disconnect(from, to) => {
                harness.reset_link(from, to);
                Injection::Disconnect
            }
            Self::Writer { from, to } => {
                let control = harness.links[&(from, to)].clone();
                let previously_blocked = control.blocked();
                control.pause_writer(true);
                Injection::Writer {
                    control,
                    previously_blocked,
                }
            }
            Self::Mailbox { ref target, delay } => {
                let cell = match target {
                    MailboxTarget::Network(node) => harness.nodes.borrow()[*node]
                        .node
                        .as_ref()
                        .context("mailbox target offline")?
                        .actor
                        .get_cell(),
                    MailboxTarget::Actor(cell) => cell.clone(),
                };
                Injection::Mailbox(slow_mailbox(&cell, delay))
            }
            Self::ProtocolPause {
                node,
                channel,
                ref point,
                preceding_hits,
            } => {
                let peer = harness.peers[node];
                let pause = match point {
                    ProtocolPoint::CommitmentAck => {
                        pause_before_commitment_ack(peer, channel, preceding_hits)
                    }
                    ProtocolPoint::ForwardFulfill => {
                        pause_before_forwarding_fulfill(peer, channel, preceding_hits)
                    }
                    ProtocolPoint::FundingPersisted => {
                        fnn::simulation::pause_after_funding_state(peer, channel, preceding_hits)
                    }
                    ProtocolPoint::ChannelIdRewritten => {
                        fnn::simulation::pause_after_channel_id_rewrite(
                            peer,
                            channel,
                            preceding_hits,
                        )
                    }
                };
                Injection::ProtocolPause(RefCell::new(pause))
            }
        })
    }
}

impl Injection {
    pub(in crate::scenario) fn hit(&self, harness: &Harness) -> bool {
        match self {
            Self::Crash(..) | Self::Partition { .. } | Self::Disconnect => true,
            Self::Chain { .. } => self.chain_hit(harness, None),
            Self::Sqlite { node, fault } => harness.disks[*node].hit() == Some(*fault),
            Self::Tcp { control, fault } => control.hit(*fault),
            Self::DelayPeerFrame {
                control,
                previously_delayed,
            } => control.delayed_frames() > *previously_delayed,
            Self::DropPeerFrame {
                control,
                previously_dropped,
                ..
            } => control.dropped_frames() > *previously_dropped,
            Self::Writer {
                control,
                previously_blocked,
            } => control.blocked() > *previously_blocked,
            Self::Mailbox(delay) => delay.hits() > 0,
            Self::ProtocolPause(pause) => pause.borrow_mut().reached(),
        }
    }

    pub(in crate::scenario) fn chain_hit(
        &self,
        harness: &Harness,
        endpoint: Option<ChainEndpoint>,
    ) -> bool {
        let Self::Chain {
            node,
            first_hit,
            request,
            fault,
        } = self
        else {
            unreachable!("only a chain injection records chain requests");
        };
        harness.chain_hits[*node][*first_hit..].iter().any(|hit| {
            hit.fault == *fault
                && endpoint.is_none_or(|endpoint| hit.endpoint == endpoint)
                && request.is_none_or(|target| target == (hit.endpoint, hit.request))
        })
    }

    pub(in crate::scenario) fn mailbox_hits(&self) -> u64 {
        match self {
            Self::Mailbox(delay) => delay.hits(),
            _ => unreachable!("only a mailbox injection records delayed messages"),
        }
    }

    pub(in crate::scenario) fn stop_after_sqlite_fault(self, harness: &mut Harness) {
        let Self::Sqlite { node, .. } = self else {
            unreachable!("expected SQLite injection");
        };
        assert!(self.hit(harness), "SQLite fault must hit before crashing");
        harness.crash(node);
        harness.disks[node].clear();
    }

    pub(in crate::scenario) fn heal(self, harness: &mut Harness) {
        match self {
            Self::Crash(node) => harness.restart(node),
            Self::Partition { from, to, one_way } => {
                harness.repair(from, to, one_way);
                harness.reset_link(from, to);
            }
            Self::Chain { node, .. } => harness.chain_access[node].set(ChainFault::Healthy),
            Self::Sqlite { node, .. } => {
                if harness.disks[node].hit().is_some() {
                    self.stop_after_sqlite_fault(harness);
                    harness.restart(node);
                } else {
                    harness.disks[node].clear();
                }
            }
            Self::Tcp { control, .. } => control.disarm(),
            Self::DelayPeerFrame { control, .. } => control.set_frame_delay(None),
            Self::DropPeerFrame {
                control,
                previously_dropped,
                from,
                to,
            } => {
                control.set_frame_loss(None);
                if control.dropped_frames() > previously_dropped {
                    // A lost application frame cannot be replayed by TCP.
                    harness.reset_link(from, to);
                }
            }
            Self::Disconnect => {}
            Self::Writer { control, .. } => control.pause_writer(false),
            Self::Mailbox(delay) => drop(delay),
            Self::ProtocolPause(pause) => drop(pause),
        }
    }
}
