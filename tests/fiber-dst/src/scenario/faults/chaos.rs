use std::collections::BTreeSet;
use std::time::Duration;

use crate::sqlite_vfs::{BatchTarget, DiskFault};
use anyhow::Result;
use fnn::fiber::channel::ChannelActorStateStore;
use fnn::fiber::types::{
    AddTlc, ChannelReadyV2, ClosingSignedV2, CommitmentSignedV2, FiberMessage,
    ReestablishChannelV2, RemoveTlc,
};
use fnn::fiber::Hash256;
use fnn::fiber_types::{HashAlgorithm, RemoveTlcFulfill, RemoveTlcReason};
use rand::rngs::StdRng;
use rand::seq::IteratorRandom;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};

use crate::harness::Harness;
use crate::host::{
    AdversarialDelayRule, FrameKindFilter, FrameLoss, FrameMatcher, InjectedWireAction, NAMES,
};

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
    protected_node: Option<usize>,
    cut: Option<Cut>,
    relays: BTreeSet<usize>,
    launched: usize,
    funding_cuts: usize,
    long_delay_rng: Option<StdRng>,
    adversarial_rng: Option<StdRng>,
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
            protected_node: None,
            cut: None,
            relays: BTreeSet::new(),
            launched: 0,
            funding_cuts: 0,
            long_delay_rng: super::long_delay_rng(seed),
            adversarial_rng: None,
        }
    }

    pub(in crate::scenario) fn with_adversarial(mut self, seed: u64) -> Self {
        self.adversarial_rng = Some(StdRng::seed_from_u64(seed ^ 0x6164_7663_6861_6f73));
        self
    }

    pub(in crate::scenario) fn protect_node(&mut self, node: Option<usize>) {
        self.protected_node = node;
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
                if active.label == "blackhole" && hit {
                    if let Target::Link(a, b) = active.target {
                        harness.reset_link(a, b);
                    }
                }
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

        if self.launched > 0
            && self
                .adversarial_rng
                .as_mut()
                .is_some_and(|rng| rng.random_ratio(1, 3))
            && self.launch_adversarial(harness, now)?
        {
            self.launched += 1;
            return Ok(());
        }

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
                        && self.protected_node != Some(node)
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

    fn launch_adversarial(&mut self, harness: &mut Harness, now: Duration) -> Result<bool> {
        let links = harness
            .links
            .keys()
            .copied()
            .filter(|&(a, b)| {
                a < b
                    && harness.link(a, b)
                    && self.protected_open != Some((a, b))
                    && self.protected_node != Some(a)
                    && self.protected_node != Some(b)
                    && !self
                        .active
                        .iter()
                        .any(|window| window.target == Target::Link(a, b))
            })
            .collect::<Vec<_>>();
        let Some(rng) = self.adversarial_rng.as_mut() else {
            return Ok(false);
        };
        let Some((a, b)) = links.into_iter().choose(rng) else {
            return Ok(false);
        };
        let (from, to) = if rng.random() { (a, b) } else { (b, a) };
        let choice = rng.random_range(0..4);

        let mut reset_after_arm = false;
        let (fault, label, length): (Fault, &'static str, u64) = match choice {
            0 => {
                const DELAY_KINDS: [FrameKindFilter; 5] = [
                    FrameKindFilter::CommitmentSignedV2,
                    FrameKindFilter::RevokeAndAckV2,
                    FrameKindFilter::AddTlc,
                    FrameKindFilter::RemoveTlc,
                    FrameKindFilter::ReestablishChannelV2,
                ];
                let kind = DELAY_KINDS[rng.random_range(0..DELAY_KINDS.len())];
                let delay_ms = rng.random_range(25..=250);
                (
                    Fault::AdversarialDelayPeerFrame {
                        from,
                        to,
                        rule: AdversarialDelayRule {
                            matcher: FrameMatcher::kind(kind),
                            duration: Duration::from_millis(delay_ms),
                            out_of_order: true,
                            persistent: false,
                        },
                    },
                    "adv-delay-ooo",
                    delay_ms + 80,
                )
            }
            1 => {
                const DUP_KINDS: [FrameKindFilter; 4] = [
                    FrameKindFilter::CommitmentSignedV2,
                    FrameKindFilter::RevokeAndAckV2,
                    FrameKindFilter::TxSignatures,
                    FrameKindFilter::ReestablishChannelV2,
                ];
                let kind = DUP_KINDS[rng.random_range(0..DUP_KINDS.len())];
                let copies = rng.random_range(2..=3);
                let max_triggers = Some(rng.random_range(1..=3));
                (
                    Fault::DuplicatePeerFrame {
                        from,
                        to,
                        matcher: FrameMatcher::kind(kind),
                        copies,
                        max_triggers,
                    },
                    "duplicate-frame",
                    rng.random_range(60..=180),
                )
            }
            2 => {
                const BLACKHOLE_KINDS: [FrameKindFilter; 3] = [
                    FrameKindFilter::RevokeAndAckV2,
                    FrameKindFilter::RemoveTlc,
                    FrameKindFilter::ReestablishChannelV2,
                ];
                let kind = BLACKHOLE_KINDS[rng.random_range(0..BLACKHOLE_KINDS.len())];
                let length = if rng.random_ratio(1, 4) {
                    rng.random_range(220..=450)
                } else {
                    rng.random_range(60..=200)
                };
                if kind == FrameKindFilter::ReestablishChannelV2 && rng.random_ratio(1, 2) {
                    reset_after_arm = true;
                }
                (
                    Fault::BlackholePeer {
                        from,
                        to,
                        matcher: FrameMatcher::kind(kind),
                    },
                    "blackhole",
                    length,
                )
            }
            _ => {
                if let Some((channel_id, nonce_json)) = find_online_channel(harness, from, to) {
                    let actions = build_protocol_fuzz_burst(rng, channel_id, &nonce_json)?;
                    (
                        Fault::FloodWire {
                            from,
                            to,
                            actions,
                            allow_malformed_disconnect: false,
                        },
                        "protocol-fuzz",
                        40,
                    )
                } else {
                    const FALLBACK_DUP: [FrameKindFilter; 4] = [
                        FrameKindFilter::CommitmentSignedV2,
                        FrameKindFilter::RevokeAndAckV2,
                        FrameKindFilter::TxSignatures,
                        FrameKindFilter::ReestablishChannelV2,
                    ];
                    let kind = FALLBACK_DUP[rng.random_range(0..FALLBACK_DUP.len())];
                    let copies = rng.random_range(2..=3);
                    (
                        Fault::DuplicatePeerFrame {
                            from,
                            to,
                            matcher: FrameMatcher::kind(kind),
                            copies,
                            max_triggers: Some(rng.random_range(1..=3)),
                        },
                        "duplicate-frame",
                        rng.random_range(60..=180),
                    )
                }
            }
        };

        harness.note(format_args!(
            "chaos {label} {}->{} duration={length}ms",
            NAMES[from], NAMES[to]
        ));
        let injection = fault.inject(harness)?;
        if reset_after_arm {
            harness.reset_link(from, to);
        }
        self.active.push(Active {
            target: Target::Link(a, b),
            ends: now + Duration::from_millis(length),
            fault: injection,
            label,
        });
        Ok(true)
    }
}

fn find_online_channel(harness: &Harness, from: usize, to: usize) -> Option<(Hash256, Value)> {
    let store = harness.store(from).ok()?;
    let peer_key = harness.peers[to];
    for channel_id in store.get_channel_ids_by_pubkey(&peer_key) {
        if !harness.channel_online(from, to, &channel_id) {
            continue;
        }
        let state = harness.channel(from, &channel_id)?;
        let valid_nonce = state
            .session
            .remote_nonce
            .clone()
            .or_else(|| state.session.bootstrap_remote_nonce.clone())?;
        let nonce_json = serde_json::to_value(ChannelReadyV2 {
            channel_id,
            next_commitment_number: 0,
            next_commitment_nonce: valid_nonce,
        })
        .ok()?
        .get("next_commitment_nonce")?
        .clone();
        return Some((channel_id, nonce_json));
    }
    None
}

fn build_protocol_fuzz_burst(
    rng: &mut StdRng,
    channel_id: Hash256,
    nonce_json: &Value,
) -> Result<Vec<InjectedWireAction>> {
    let count = rng.random_range(4..=8);
    let now_ms = fnn::now_timestamp_as_millis_u64();
    let mut actions = Vec::with_capacity(count);
    for idx in 0..count {
        let frame = match rng.random_range(0..6) {
            // Zero-amount AddTlc.
            0 => FiberMessage::add_tlc(AddTlc {
                channel_id,
                tlc_id: 90_000 + rng.random_range(0..10_000),
                amount: 0,
                payment_hash: Hash256::from(rng.random::<[u8; 32]>()),
                expiry: now_ms.saturating_add(3_600_000),
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            }),
            // Expired AddTlc.
            1 => FiberMessage::add_tlc(AddTlc {
                channel_id,
                tlc_id: 100_000 + rng.random_range(0..10_000),
                amount: 100_000_000,
                payment_hash: Hash256::from(rng.random::<[u8; 32]>()),
                expiry: 1,
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            }),
            // Nonexistent-id RemoveTlc.
            2 => FiberMessage::remove_tlc(RemoveTlc {
                channel_id,
                tlc_id: 900_000 + rng.random_range(0..10_000),
                reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: Hash256::from(rng.random::<[u8; 32]>()),
                }),
            }),
            // Bogus commitment_number CommitmentSignedV2.
            3 => {
                let sig_bytes = [0x11u8; 32];
                let forged_cs: CommitmentSignedV2 = serde_json::from_value(json!({
                    "channel_id": channel_id,
                    "commitment_number": 99_000u64 + idx as u64,
                    "funding_tx_partial_signature": sig_bytes,
                    "funding_nonce": nonce_json.clone(),
                    "revocation_nonce": nonce_json.clone(),
                }))?;
                FiberMessage::commitment_signed_v2(forged_cs)
            }
            // Unsolicited ClosingSignedV2.
            4 => {
                let sig_bytes = [0x22u8; 32];
                let unsolicited_close: ClosingSignedV2 = serde_json::from_value(json!({
                    "channel_id": channel_id,
                    "partial_signature": sig_bytes,
                }))?;
                FiberMessage::closing_signed_v2(unsolicited_close)
            }
            // Unsolicited ReestablishChannelV2.
            _ => {
                let unsolicited_reestablish: ReestablishChannelV2 =
                    serde_json::from_value(json!({
                        "channel_id": channel_id,
                        "next_commitment_number": 99_000u64 + idx as u64,
                        "next_ack_number": 99_000u64 + idx as u64,
                        "next_local_commitment_nonce": nonce_json.clone(),
                    }))?;
                FiberMessage::reestablish_channel_v2(unsolicited_reestablish)
            }
        };
        actions.push(InjectedWireAction::SendFrame(frame));
    }
    Ok(actions)
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
