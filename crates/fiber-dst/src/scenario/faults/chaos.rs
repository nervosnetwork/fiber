use std::collections::BTreeSet;
use std::time::Duration;

use crate::sqlite_vfs::{BatchTarget, DiskFault};
use anyhow::Result;
use fnn::fiber::Hash256;
use rand::rngs::StdRng;
use rand::seq::IteratorRandom;
use rand::{Rng, SeedableRng};

use crate::harness::Harness;
use crate::host::{FrameLoss, NAMES};

use super::{Fault, Injection, ProtocolPoint};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Node(usize),
    Link(usize, usize),
}

struct Active {
    target: Target,
    ends: Duration,
    fault: Injection,
    label: &'static str,
}

struct Cut {
    node: usize,
    channel: Hash256,
    point: ProtocolPoint,
    expires: Duration,
    fault: Injection,
}

pub(in crate::scenario) struct Chaos {
    rng: StdRng,
    next: Duration,
    stop: Option<Duration>,
    opening_deadline: Option<Duration>,
    active: Vec<Active>,
    protected_open: Option<(usize, usize)>,
    cut: Option<Cut>,
    relays: BTreeSet<usize>,
    launched: usize,
    funding_cuts: usize,
    long_delay_rng: Option<StdRng>,
}

impl Chaos {
    pub(in crate::scenario) fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed ^ 0x6368_616f_7364_7374);
        Self {
            next: Duration::from_millis(rng.random_range(5..=30)),
            rng,
            stop: None,
            opening_deadline: None,
            active: Vec::new(),
            protected_open: None,
            cut: None,
            relays: BTreeSet::new(),
            launched: 0,
            funding_cuts: 0,
            long_delay_rng: super::long_delay_rng(seed),
        }
    }

    pub(in crate::scenario) fn protect_open(&mut self, a: usize, b: usize) {
        self.protected_open = Some((a.min(b), a.max(b)));
        self.opening_deadline = None;
    }

    pub(in crate::scenario) fn unprotect_open(&mut self) {
        self.protected_open = None;
    }

    pub(in crate::scenario) fn open_ready(&self, a: usize, b: usize) -> bool {
        self.active
            .iter()
            .all(|active| !blocks_open(active.target, a, b))
    }

    pub(in crate::scenario) fn after_accept(
        &mut self,
        harness: &mut Harness,
        a: usize,
        b: usize,
        channel: Hash256,
    ) -> Result<()> {
        self.protected_open = None;
        self.opening_deadline = Some(harness.elapsed() + Duration::from_millis(250));
        if self.cut.is_none() && (self.funding_cuts == 0 || self.rng.random_ratio(1, 4)) {
            let node = if self.rng.random() { a } else { b };
            let preceding_hits = if self.funding_cuts == 0 {
                0
            } else {
                self.rng.random_range(0..3)
            };
            self.arm_cut(
                harness,
                node,
                channel,
                ProtocolPoint::FundingPersisted,
                preceding_hits,
            )?;
            self.funding_cuts += 1;
        }
        Ok(())
    }

    pub(in crate::scenario) fn finish_open(&mut self, harness: &Harness) {
        self.opening_deadline = None;
        if let Some(cut) = self.cut.take() {
            harness.note(format_args!(
                "chaos missed persisted boundary node={} channel={}",
                NAMES[cut.node], cut.channel
            ));
        }
    }

    pub(in crate::scenario) fn arm_payment_cut(
        &mut self,
        harness: &mut Harness,
        channel: Hash256,
        node: usize,
        relay: bool,
    ) -> Result<()> {
        if self.cut.is_none() {
            let point = if relay && self.rng.random() {
                ProtocolPoint::ForwardFulfill
            } else {
                ProtocolPoint::CommitmentAck
            };
            let preceding_hits = self.rng.random_range(0..3);
            self.arm_cut(harness, node, channel, point, preceding_hits)?;
        }
        Ok(())
    }

    fn arm_cut(
        &mut self,
        harness: &mut Harness,
        node: usize,
        channel: Hash256,
        point: ProtocolPoint,
        preceding_hits: usize,
    ) -> Result<()> {
        harness.note(format_args!(
            "chaos arm {point:?} node={} channel={channel} occurrence={}",
            NAMES[node],
            preceding_hits + 1
        ));
        self.cut = Some(Cut {
            node,
            channel,
            point,
            expires: harness.elapsed() + Duration::from_secs(1),
            fault: Fault::ProtocolPause {
                node,
                channel,
                point,
                preceding_hits,
            }
            .inject(harness)?,
        });
        Ok(())
    }

    pub(in crate::scenario) fn arm_sqlite_batch(
        &mut self,
        harness: &mut Harness,
        node: usize,
        fault: DiskFault,
        batch: BatchTarget,
        expires: Duration,
    ) -> Result<()> {
        self.active.push(Active {
            target: Target::Node(node),
            ends: expires,
            fault: Fault::Sqlite {
                node,
                fault,
                batch: Some(batch),
            }
            .inject(harness)?,
            label: "sqlite-batch",
        });
        Ok(())
    }

    pub(in crate::scenario) fn stop_at(&mut self, time: Duration) {
        self.stop = Some(time);
    }

    pub(in crate::scenario) fn quiescent(&self, now: Duration) -> bool {
        self.stop.is_some_and(|stop| now >= stop) && self.active.is_empty() && self.cut.is_none()
    }

    pub(in crate::scenario) fn fault_active(&self) -> bool {
        !self.active.is_empty()
    }

    pub(in crate::scenario) fn set_relays(&mut self, relays: BTreeSet<usize>) {
        self.relays = relays;
    }

    pub(in crate::scenario) fn advance(&mut self, harness: &mut Harness) -> Result<()> {
        let now = harness.elapsed();
        let mut index = 0;
        while index < self.active.len() {
            if self.active[index].ends <= now {
                let active = self.active.remove(index);
                let hit = active.fault.hit(harness);
                active.fault.heal(harness);
                harness.note(format_args!("chaos end {} hit={hit}", active.label));
            } else {
                index += 1;
            }
        }

        let healing = self.stop.is_some_and(|stop| now >= stop);
        if let Some(cut) = &self.cut {
            let hit = cut.fault.hit(harness);
            if hit || healing || cut.expires <= now {
                let cut = self.cut.take().expect("armed cut");
                if hit && harness.node_available(cut.node) {
                    harness.note(format_args!(
                        "chaos hit {:?} node={} channel={}",
                        cut.point, NAMES[cut.node], cut.channel
                    ));
                    self.crash(harness, cut.node, now)?;
                }
                cut.fault.heal(harness);
            }
        }
        if healing
            || now < self.next
            || self
                .opening_deadline
                .is_some_and(|deadline| now >= deadline)
        {
            return Ok(());
        }
        self.next = now + Duration::from_millis(self.rng.random_range(35..=85));

        // Start with a crash so even a short startup exercises recovery. Later
        // choices use the same seeded stream through funding and payment waves.
        let action = if self.launched == 0 {
            0
        } else {
            self.rng.random_range(0..=4)
        };
        if action <= 1 {
            let relay = action == 1 && !self.relays.is_empty();
            let candidates = (0..harness.peers.len())
                .filter(|&node| {
                    harness.node_available(node)
                        && !self
                            .active
                            .iter()
                            .any(|active| active.target == Target::Node(node))
                        && !self
                            .protected_open
                            .is_some_and(|(a, b)| node == a || node == b)
                        && (!relay || self.relays.contains(&node))
                })
                .collect::<Vec<_>>();
            if let Some(node) = candidates.into_iter().choose(&mut self.rng) {
                self.crash(harness, node, now)?;
                self.launched += 1;
            }
            return Ok(());
        }

        let links = harness
            .links
            .keys()
            .copied()
            .filter(|&(a, b)| {
                a < b
                    && harness.link(a, b)
                    && self.protected_open != Some((a, b))
                    && !self
                        .active
                        .iter()
                        .any(|window| window.target == Target::Link(a, b))
            })
            .collect::<Vec<_>>();
        let Some((a, b)) = links.into_iter().choose(&mut self.rng) else {
            return Ok(());
        };
        let (from, to) = if self.rng.random() { (a, b) } else { (b, a) };
        let delay = self.long_delay_rng.as_mut().and_then(|rng| {
            rng.random_ratio(1, 8).then(|| {
                let after = rng.random_range(0..3);
                super::sample_long_delay(rng, after)
            })
        });
        let (fault, label, length) = match (action, delay) {
            (_, Some(rule)) => {
                self.long_delay_rng = None;
                (
                    Fault::DelayPeerFrame { from, to, rule },
                    "delay-frame",
                    rule.duration.as_millis() as u64,
                )
            }
            (2, None) => (
                Fault::Partition {
                    from,
                    to,
                    one_way: self.rng.random(),
                },
                "partition",
                self.rng.random_range(25..=120),
            ),
            (3, None) => (
                Fault::DropPeerFrame {
                    from,
                    to,
                    rule: FrameLoss::After(self.rng.random_range(0..3)),
                },
                "drop-frame",
                self.rng.random_range(40..=120),
            ),
            _ => (Fault::Disconnect(from, to), "disconnect", 15),
        };
        harness.note(format_args!(
            "chaos {label} {}->{} duration={length}ms",
            NAMES[from], NAMES[to]
        ));
        self.active.push(Active {
            target: Target::Link(a, b),
            ends: now + Duration::from_millis(length),
            fault: fault.inject(harness)?,
            label,
        });
        self.launched += 1;
        Ok(())
    }

    fn crash(&mut self, harness: &mut Harness, node: usize, now: Duration) -> Result<()> {
        let length = self.rng.random_range(20..=190);
        let generation = harness.nodes.borrow()[node].generation;
        harness.note(format_args!(
            "chaos crash {} generation={generation} duration={length}ms relay={}",
            NAMES[node],
            self.relays.contains(&node)
        ));
        self.active.push(Active {
            target: Target::Node(node),
            ends: now + Duration::from_millis(length),
            fault: Fault::Crash(node).inject(harness)?,
            label: "crash",
        });
        Ok(())
    }
}

fn blocks_open(target: Target, a: usize, b: usize) -> bool {
    target == Target::Link(a.min(b), a.max(b))
        || target == Target::Node(a)
        || target == Target::Node(b)
}

#[cfg(test)]
mod tests {
    use super::{blocks_open, Target};

    #[test]
    fn opening_waits_for_existing_endpoint_and_link_faults() {
        for target in [Target::Node(2), Target::Node(5), Target::Link(2, 5)] {
            assert!(blocks_open(target, 5, 2));
        }
        for target in [Target::Node(1), Target::Link(1, 2), Target::Link(5, 6)] {
            assert!(!blocks_open(target, 2, 5));
        }
    }
}
