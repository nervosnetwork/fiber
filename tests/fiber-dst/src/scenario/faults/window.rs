use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::Rng;

use crate::harness::{Harness, TICK};
use crate::host::{FrameDelay, NAMES};

use super::{Fault, Injection, MailboxTarget};

pub(in crate::scenario) struct FaultWindow {
    start: usize,
    pub(in crate::scenario) end: usize,
    fault: Fault,
    active: Option<Injection>,
}

pub(in crate::scenario) fn schedule_fault_windows(
    rng: &mut StdRng,
    links: &[(usize, usize)],
    nodes: usize,
    slot_probability: i32,
) -> Vec<FaultWindow> {
    let crash = rng.random_range(0..nodes);
    let mailbox = (crash + rng.random_range(1..nodes)) % nodes;
    let (a, b) = links[rng.random_range(0..links.len())];
    let mut faults = [
        Fault::Crash(crash),
        Fault::Partition {
            from: a,
            to: b,
            one_way: false,
        },
        Fault::Mailbox {
            target: MailboxTarget::Network(mailbox),
            delay: TICK,
        },
    ];
    faults.shuffle(rng);
    let mut windows: Vec<_> = faults
        .into_iter()
        .enumerate()
        .map(|(index, mut fault)| {
            let start = index * 4 + rng.random_range(2..=5);
            let length = rng.random_range(9..=15);
            if let Fault::Mailbox { delay, .. } = &mut fault {
                *delay = TICK * length as u32 / 2;
            }
            FaultWindow {
                start,
                end: start + length,
                fault,
                active: None,
            }
        })
        .collect();
    if slot_probability < 100 {
        windows.retain(|_| rng.random_range(0..100) < slot_probability);
    }
    windows
}

impl FaultWindow {
    pub(in crate::scenario) fn delay_frame(
        from: usize,
        to: usize,
        start: usize,
        rule: FrameDelay,
    ) -> Self {
        Self {
            start,
            end: start + (rule.duration.as_millis() / TICK.as_millis()) as usize,
            fault: Fault::DelayPeerFrame { from, to, rule },
            active: None,
        }
    }

    pub(in crate::scenario) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(in crate::scenario) fn advance(
        &mut self,
        tick: usize,
        harness: &mut Harness,
    ) -> Result<()> {
        if tick == self.start {
            match self.fault {
                Fault::Partition { from, to, .. } => harness.note(format_args!(
                    "swarm partition {}-{}",
                    NAMES[from], NAMES[to]
                )),
                Fault::Mailbox {
                    target: MailboxTarget::Network(node),
                    delay,
                } => {
                    harness.note(format_args!("swarm slow mailbox {} {delay:?}", NAMES[node]));
                }
                Fault::DelayPeerFrame { from, to, rule } => harness.note(format_args!(
                    "swarm delay-frame {}->{} duration={}ms",
                    NAMES[from],
                    NAMES[to],
                    rule.duration.as_millis()
                )),
                _ => {}
            }
            self.active = Some(self.fault.inject(harness)?);
        }
        if tick == self.end {
            let active = self.active.take().context("fault window was never armed")?;
            if let Fault::Mailbox {
                target: MailboxTarget::Network(node),
                ..
            } = self.fault
            {
                harness.note(format_args!(
                    "swarm heal mailbox {} hits={}",
                    NAMES[node],
                    active.mailbox_hits()
                ));
            }
            if matches!(self.fault, Fault::DelayPeerFrame { .. }) {
                harness.note(format_args!(
                    "swarm end delay-frame hit={}",
                    active.hit(harness)
                ));
            }
            active.heal(harness);
            if let Fault::Partition { from, to, .. } = self.fault {
                harness.note(format_args!("swarm repair {}-{}", NAMES[from], NAMES[to]));
            }
        }
        Ok(())
    }
}
