use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fnn::fiber::Hash256;
use fnn::simulation::{self, BatchTarget, DiskFault};
use rand::seq::{IndexedRandom, IteratorRandom, SliceRandom};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::host::{TcpFault, NAMES};

use crate::scenario::faults::{
    long_delay_rng, sample_long_delay, schedule_fault_windows, Chaos, Fault, FaultWindow,
};
use crate::scenario::funding::{mutable_key, BatchFault, FundingOrder, OpeningFault};
use crate::scenario::{RandomFaultPolicy, TICK};

use super::{Driver, Edge, Kind, Ledger, Order, FUNDING_AMOUNT, MPP_AMOUNT, RELAY_FEE_PPM};

/// The seeded peer graph contains a two-path core and two peers for each added node.
pub(crate) struct Topology {
    pub(crate) links: Vec<(usize, usize)>,
    core: [usize; 4],
    nodes: usize,
}

impl Topology {
    pub(crate) fn generate(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed ^ 0x746f_706f_6c6f_6779);
        let nodes = rng.random_range(6..=NAMES.len());
        let mut labels: Vec<_> = (0..nodes).collect();
        labels.shuffle(&mut rng);
        let [a, b, c, d] = labels[..4].try_into().expect("at least four nodes");
        let mut links = vec![link(a, b), link(a, c), link(b, d), link(c, d)];
        for (index, &node) in labels.iter().enumerate().skip(4) {
            links.extend(
                labels[..index]
                    .choose_multiple(&mut rng, 2)
                    .map(|&peer| link(node, peer)),
            );
        }
        links.sort_unstable();
        Self {
            links,
            core: [a, b, c, d],
            nodes,
        }
    }

    pub(crate) fn lsp_node(&self, seed: u64) -> usize {
        // Pick one of the two diamond-core relay nodes so LSP has both inbound and outbound
        // multi-hop paths across the full 6-8 node mesh.
        self.core[1 + (seed as usize % 2)]
    }

    fn pairs(&self, adjacent: bool) -> Vec<(usize, usize)> {
        let directed = (0..self.nodes).flat_map(|from| {
            (0..self.nodes)
                .filter(move |&to| from != to)
                .map(move |to| (from, to))
        });
        directed
            .filter(|&(from, to)| self.links.binary_search(&link(from, to)).is_ok() == adjacent)
            .collect()
    }
}

impl Driver {
    pub(in crate::scenario) fn random(
        &mut self,
        seed: u64,
        topology: &Topology,
        payments: usize,
        phases: usize,
        fault_policy: RandomFaultPolicy,
        lsp: bool,
    ) -> Result<()> {
        let mut setup = StdRng::seed_from_u64(seed ^ 0x6361_6d70_7365_7475);
        let mut workload = StdRng::seed_from_u64(seed ^ 0x6361_6d70_776f_726b);
        let mut faults = StdRng::seed_from_u64(seed ^ 0x6361_6d70_6661_756c);
        let mut invoices = StdRng::seed_from_u64(seed ^ 0x6361_6d70_696e_766f);
        let mut parallel = StdRng::seed_from_u64(seed ^ 0x7061_7261_6675_6e64);
        let links = &topology.links;
        let named_links: Vec<_> = links.iter().map(|&(a, b)| (NAMES[a], NAMES[b])).collect();
        self.runtime.note(format_args!(
            "random topology nodes={} links={named_links:?}",
            topology.nodes
        ));
        // External towers must receive commitment snapshots as channels are opened.
        for tower in self
            .runtime
            .watchtowers
            .as_ref()
            .context("watchtower controls")?
        {
            tower.enabled.send_replace(true);
        }
        let continuous = fault_policy != RandomFaultPolicy::Windows;
        if fault_policy == RandomFaultPolicy::Continuous {
            self.chaos = Some(Chaos::new(seed));
            self.runtime.note("chaos startup starts");
            self.until("peers recover during randomized startup", |s| {
                Ok((s.runtime.elapsed() >= Duration::from_millis(100)
                    && links.iter().all(|&(a, b)| s.runtime.link(a, b)))
                .then_some(()))
            })?;
            self.runtime.note("chaos startup links recovered");
        } else {
            self.random_startup(links, &mut setup)?;
        }

        let [a, b, c, d] = topology.core;
        let core_links = [(a, b), (a, c), (b, d), (c, d)];
        let mut extra_links: Vec<_> = links
            .iter()
            .copied()
            .filter(|(a, b)| !topology.core.contains(a) || !topology.core.contains(b))
            .collect();
        extra_links.shuffle(&mut setup);
        let batch = if fault_policy == RandomFaultPolicy::Continuous && parallel.random_ratio(1, 3)
        {
            let index = parallel.random_range(0..extra_links.len());
            let (from, to) = extra_links[index];
            self.runtime.note(format_args!(
                "parallel funding selected {}-{}",
                NAMES[from], NAMES[to]
            ));
            Some((index, &mut parallel))
        } else {
            None
        };
        let mut funding_txs = BTreeSet::new();
        self.runtime.note("random funding core starts");
        let mut edges = self.random_fund(
            core_links.map(|(from, to)| (from, to, FUNDING_AMOUNT)),
            &mut funding_txs,
            None,
        )?;
        if self.chaos.is_none() {
            simulation::observe_tlc_settlements();
            let mut ledger = Ledger::new(&edges);
            for (name, orders) in [
                (
                    "random MPP baseline",
                    [
                        order(0, a, d, Kind::Mpp, MPP_AMOUNT),
                        order(0, d, a, Kind::Mpp, MPP_AMOUNT),
                    ],
                ),
                (
                    "random multihop baseline",
                    [
                        order(0, b, c, Kind::Multihop, 100_000),
                        order(0, c, b, Kind::Multihop, 100_000),
                    ],
                ),
            ] {
                let mut warmup = self.prepare_payments(orders, &mut invoices, &mut ledger)?;
                let warm = self.drive_payment_wave(name, &mut warmup, &mut [], &mut ledger)?;
                ensure!(
                    warm.peak >= 2,
                    "{name} did not exercise concurrent payments"
                );
                ledger.check_payments(self, &warmup, true)?;
            }
            ledger.check_consumed()?;
        }

        self.runtime.note("random funding expansion starts");
        let expansion = extra_links.into_iter().map(|(a, b)| {
            let (from, to) = if setup.random() { (a, b) } else { (b, a) };
            let amount = FUNDING_AMOUNT * setup.random_range(90..=100) / 100;
            (from, to, amount)
        });
        edges.extend(self.random_fund(expansion, &mut funding_txs, batch)?);
        if self.chaos.is_some() {
            self.until(
                "all nodes are available for recording payment intents",
                |s| {
                    Ok((0..s.runtime.peers.len())
                        .all(|node| s.runtime.node_available(node))
                        .then_some(()))
                },
            )?;
            simulation::observe_tlc_settlements();
        }
        // Core channels carry the settled baseline balances into the expanded mesh.
        for edge in &mut edges {
            *edge = self.opened_edge(edge.sender, edge.receiver, edge.channel, edge.fee)?;
        }
        // When LSP is enabled, register external fiber-lsp-sdk wallets and open their private
        // hosted channels BEFORE the public payment workload so LSP/tenant state also endures
        // public chaos crashes, partitions, and SQLite faults.
        let random_lsp = if lsp {
            let saved_chaos = self.chaos.take();
            let state = self.bootstrap_random_lsp(topology.nodes)?;
            self.chaos = saved_chaos;
            Some(state)
        } else {
            None
        };
        let mut ledger = Ledger::new(&edges);

        let direct = topology.pairs(true);
        let multihop = topology.pairs(false);
        let fault_probability = [25, 50, 75][faults.random_range(0..3)];
        if continuous {
            self.runtime.note(format_args!(
                "random workload payments={payments} phases={phases} fault_policy=continuous"
            ));
        } else {
            self.runtime.note(format_args!(
                "random workload payments={payments} phases={phases} fault_slot_probability={fault_probability}%"
            ));
        }
        let mut reconciled = 0;
        let mut settled = 0;
        let mut overlaps = 0;
        let mut peak = 0;
        if continuous {
            let mut chaos = self.chaos.take().unwrap_or_else(|| Chaos::new(seed));
            let mut orders = Vec::with_capacity(payments);
            let mut start = 0;
            for phase in 0..phases {
                let count = payments / phases + usize::from(phase < payments % phases);
                self.runtime.note(format_args!(
                    "chaos wave {}/{} orders={count} starts_tick={start}",
                    phase + 1,
                    phases
                ));
                for index in 0..count {
                    let mut payment = sample_order(index, &mut workload, &direct, &multihop);
                    payment.tick += start;
                    orders.push(payment);
                }
                start = orders.last().expect("nonempty wave").tick + workload.random_range(4..=12);
            }
            let mut storage_rng = StdRng::seed_from_u64(seed ^ 0x0073_746f_7261_6765);
            let storage_order = if storage_rng.random_ratio(1, 8) {
                orders
                    .iter()
                    .filter(|order| order.tick <= 10 && matches!(order.kind, Kind::Direct))
                    .choose(&mut storage_rng)
                    .map(|order| (order.from, order.to))
            } else {
                None
            };
            let mut batch = self.prepare_payments(orders, &mut invoices, &mut ledger)?;
            let stop = self.runtime.elapsed() + TICK * u32::try_from(start + 20)?;
            let edge = edges.choose(&mut faults).expect("funded topology");
            let node = if faults.random() {
                edge.sender
            } else {
                edge.receiver
            };
            chaos.arm_payment_cut(&mut self.runtime, edge.channel, node, true)?;
            if let Some((from, target)) = storage_order.filter(|(_, target)| *target != node) {
                let channel = edges
                    .iter()
                    .find(|edge| link(edge.sender, edge.receiver) == link(from, target))
                    .context("direct payment has no funded edge")?
                    .channel;
                let fault = [DiskFault::WriteIo, DiskFault::DiskFull, DiskFault::SyncIo]
                    [storage_rng.random_range(0..3)];
                let expires = self.runtime.elapsed() + Duration::from_millis(250);
                self.runtime.note(format_args!(
                    "chaos arm sqlite-batch {} {fault:?} channel={channel}",
                    NAMES[target]
                ));
                chaos.arm_sqlite_batch(
                    &mut self.runtime,
                    target,
                    fault,
                    BatchTarget {
                        put_prefix: Some(mutable_key(channel)),
                        delete_prefix: None,
                    },
                    expires,
                )?;
            }
            chaos.stop_at(stop);
            self.chaos = Some(chaos);
            let outcome =
                self.drive_payment_wave("continuous workload", &mut batch, &mut [], &mut ledger)?;
            settled = ledger.check_payments(self, &batch, false)?;
            reconciled = batch.len();
            overlaps = usize::from(outcome.fault_overlapped_payment);
            peak = outcome.peak;
            self.chaos.take();
            self.runtime.note("chaos recovery probes start");
        } else {
            let mut delay_rng = long_delay_rng(seed);
            let delay_phase = delay_rng.as_mut().map(|rng| rng.random_range(0..phases));
            for phase in 0..phases {
                let count = payments / phases + usize::from(phase < payments % phases);
                let orders =
                    (0..count).map(|index| sample_order(index, &mut workload, &direct, &multihop));
                let mut batch = self.prepare_payments(orders, &mut invoices, &mut ledger)?;
                let mut windows =
                    schedule_fault_windows(&mut faults, links, topology.nodes, fault_probability);
                if Some(phase) == delay_phase {
                    let rng = delay_rng.as_mut().expect("seeded delay RNG");
                    let (from, to, tick) = batch
                        .iter()
                        .map(|payment| &payment.order)
                        .filter(|order| matches!(order.kind, Kind::Direct))
                        .choose(rng)
                        .map_or_else(
                            || {
                                let &(a, b) = links.choose(rng).expect("funded topology");
                                if rng.random() {
                                    (a, b, 0)
                                } else {
                                    (b, a, 0)
                                }
                            },
                            |order| (order.from, order.to, order.tick),
                        );
                    windows.push(FaultWindow::delay_frame(
                        from,
                        to,
                        tick,
                        sample_long_delay(rng, 0),
                    ));
                }
                let name = format!("random wave {}/{}", phase + 1, phases);
                self.runtime.note(format_args!(
                    "{name} orders={count} fault_windows={}",
                    windows.len()
                ));
                let outcome =
                    self.drive_payment_wave(&name, &mut batch, &mut windows, &mut ledger)?;
                settled += ledger.check_payments(self, &batch, false)?;
                reconciled += batch.len();
                overlaps += usize::from(outcome.fault_overlapped_payment);
                peak = peak.max(outcome.peak);
            }
        }

        let mut lsp_summary = if let Some(lsp_state) = &random_lsp {
            let lsp_payments = (payments / 2).clamp(6, 20);
            let summary = self.run_random_lsp_workload(
                seed,
                lsp_state,
                topology.nodes,
                lsp_payments,
                continuous,
            )?;
            // Multi-hop public <-> SDK payments shift balances on the public mesh and emit
            // hosted-channel TLC settlements; discard those observations, keep only live
            // online edges (so previously closed edges are not re-audited against the fresh
            // recovery ledger), and re-snapshot their balances before recovery probes.
            let _ = simulation::take_tlc_settlements();
            let mut live_edges = Vec::new();
            for edge in &edges {
                if self
                    .runtime
                    .channel_online(edge.sender, edge.receiver, &edge.channel)
                {
                    live_edges.push(self.opened_edge(
                        edge.sender,
                        edge.receiver,
                        edge.channel,
                        edge.fee,
                    )?);
                }
            }
            edges = live_edges;
            ledger = Ledger::new(&edges);
            Some(summary)
        } else {
            None
        };

        let probes = [
            order(0, a, b, Kind::Direct, 10_000),
            order(0, d, c, Kind::Direct, 10_000),
            order(0, b, c, Kind::Multihop, 10_000),
            order(0, c, b, Kind::Multihop, 10_000),
        ];
        let mut recovery = self.prepare_payments(probes, &mut invoices, &mut ledger)?;
        self.drive_payment_wave("random recovery", &mut recovery, &mut [], &mut ledger)?;
        ledger.check_payments(self, &recovery, true)?;
        ledger.check_consumed()?;
        let live_edges: Vec<_> = edges
            .iter()
            .filter(|edge| {
                self.runtime
                    .channel_online(edge.sender, edge.receiver, &edge.channel)
            })
            .collect();
        let edge = live_edges
            .choose(&mut faults)
            .context("no online channel for on-chain probe")?;
        let (closer, peer) = if faults.random() {
            (edge.sender, edge.receiver)
        } else {
            (edge.receiver, edge.sender)
        };
        let endpoints = if self
            .runtime
            .channel(closer, &edge.channel)
            .is_some_and(|state| state.get_local_balance() > 0)
        {
            [closer, peer]
        } else {
            [peer, closer]
        };
        self.start_watchtowers(endpoints)?;
        self.settle_held_payment(
            edge.channel,
            endpoints,
            Hash256::from(faults.random::<[u8; 32]>()),
        )?;
        if let Some(lsp_state) = &random_lsp {
            if continuous {
                let settled_onchain =
                    self.run_random_lsp_offline_force_close_probe(seed, lsp_state, topology.nodes)?;
                if let Some(summary) = &mut lsp_summary {
                    summary.self_funded_watchtower = settled_onchain;
                }
            }
            let public_nodes: Vec<usize> = (0..topology.nodes).collect();
            self.verify_lsp_indexes(&public_nodes, &lsp_state.tenant_ids)?;
        }
        ensure!(reconciled == payments);
        if let Some(lsp_summary) = lsp_summary {
            self.runtime.note(format_args!(
                "random reconciled workload={reconciled} successful={settled} phases={phases} fault_overlaps={overlaps} peak_inflight={peak} recovery_probes={} lsp_payments={} lsp_succeeded={} lsp_concurrent={} lsp_long_offline={} lsp_crashes={} lsp_node_crashes={} lsp_duplicates={} lsp_responses_dropped={} lsp_self_funded_watchtower={}",
                recovery.len(),
                lsp_summary.payments,
                lsp_summary.succeeded,
                lsp_summary.concurrent_payments,
                lsp_summary.expired_long_offline,
                lsp_summary.crashes_injected,
                lsp_summary.lsp_crashes_injected,
                lsp_summary.duplicates_injected,
                lsp_summary.responses_dropped,
                lsp_summary.self_funded_watchtower,
            ));
        } else {
            self.runtime.note(format_args!(
                "random reconciled workload={reconciled} successful={settled} phases={phases} fault_overlaps={overlaps} peak_inflight={peak} recovery_probes={}",
                recovery.len()
            ));
        }
        Ok(())
    }

    fn random_fund(
        &mut self,
        orders: impl IntoIterator<Item = (usize, usize, u128)>,
        funding_txs: &mut BTreeSet<Hash256>,
        mut batch: Option<(usize, &mut StdRng)>,
    ) -> Result<Vec<Edge>> {
        let mut edges = Vec::new();
        for (index, (from, to, amount)) in orders.into_iter().enumerate() {
            if batch.as_ref().is_some_and(|(slot, _)| *slot == index) {
                let (_, rng) = batch.take().expect("selected parallel funding slot");
                let count = rng.random_range(2..=6);
                let victim = rng.random_range(0..count);
                let fault = match rng.random_range(0..3) {
                    0 => BatchFault::DropUpdate(victim),
                    1 => BatchFault::Chaos(victim),
                    _ => BatchFault::SlowChain {
                        index: victim,
                        ticks: rng.random_range(4..=15),
                    },
                };
                let orders = (0..count)
                    .map(|_| {
                        let (opener, acceptor) = if rng.random() { (from, to) } else { (to, from) };
                        FundingOrder {
                            opener,
                            acceptor,
                            amount: amount * rng.random_range(90..=100) / 100,
                            gap_ticks: rng.random_range(0..=2),
                        }
                    })
                    .collect();
                edges.extend(self.parallel_funding(orders, fault, RELAY_FEE_PPM, funding_txs)?);
                continue;
            }
            let inject_ckb = index == 0 && self.chaos.is_none();
            let fault = if inject_ckb {
                OpeningFault::CkbOutage
            } else {
                OpeningFault::None
            };
            let edge = self
                .open_between(from, to, amount, RELAY_FEE_PPM, true, fault)
                .with_context(|| format!("opening {}-{}", NAMES[from], NAMES[to]))?;
            self.check_funded_pair(from, to, std::slice::from_ref(&edge), funding_txs)?;
            self.runtime.note(format_args!(
                "random funded {}-{} ckb_fault={inject_ckb} {}",
                NAMES[from], NAMES[to], edge.channel
            ));
            edges.push(edge);
        }
        self.step()?; // Deliver remaining channel advertisements before routing.
        Ok(edges)
    }

    fn random_startup(&mut self, links: &[(usize, usize)], setup: &mut StdRng) -> Result<()> {
        let &(from, to) = links.choose(setup).expect("non-empty topology");
        self.runtime.note(format_args!(
            "random startup arm TCP Dial {}->{}",
            NAMES[from], NAMES[to]
        ));
        let dial = Fault::Tcp {
            from,
            to,
            fault: TcpFault::Dial,
            reconnect: false,
        }
        .inject(&mut self.runtime)?;
        self.until("first boot encounters dial refusal", |s| {
            let nodes = s.runtime.nodes.borrow();
            Ok((nodes[from].node.is_some() && dial.hit(&s.runtime)).then_some(()))
        })?;
        ensure!(
            !self.runtime.link(from, to),
            "refused boot dial reported an active link"
        );
        self.runtime.note(format_args!(
            "random startup hit TCP Dial {}->{}",
            NAMES[from], NAMES[to]
        ));
        dial.heal(&mut self.runtime);
        let crash = Fault::Crash(from).inject(&mut self.runtime)?;
        for _ in 0..setup.random_range(4..=8) {
            self.step()?;
        }
        ensure!(
            self.runtime.nodes.borrow()[from].node.is_none(),
            "crashed node restarted before requested"
        );
        crash.heal(&mut self.runtime);
        self.until("every startup peer link is online", |s| {
            Ok(links
                .iter()
                .all(|&(a, b)| s.runtime.link(a, b))
                .then_some(()))
        })?;
        let nodes = self.runtime.nodes.borrow();
        ensure!(
            nodes
                .iter()
                .enumerate()
                .all(|(i, node)| node.node.is_some()
                    && node.generation == if i == from { 2 } else { 1 }),
            "unexpected node generation at end of startup"
        );
        self.runtime.note(format_args!(
            "random startup recovered {} generation 2",
            NAMES[from]
        ));
        Ok(())
    }
}

fn sample_order(
    index: usize,
    rng: &mut StdRng,
    direct: &[(usize, usize)],
    multihop: &[(usize, usize)],
) -> Order {
    let kind = [Kind::Direct, Kind::Multihop, Kind::Mpp][rng.random_range(0..3)];
    let pairs = if kind == Kind::Direct {
        direct
    } else {
        multihop
    };
    let &(from, to) = pairs.choose(rng).expect("both pair classes exist");
    let amount = if kind == Kind::Mpp && rng.random_range(0..8) == 0 {
        MPP_AMOUNT
    } else {
        rng.random_range(100_000..1_000_000)
    };
    order(index.saturating_sub(3) * 2, from, to, kind, amount)
}

fn order(tick: usize, from: usize, to: usize, kind: Kind, amount: u128) -> Order {
    Order {
        tick,
        from,
        to,
        kind,
        amount,
    }
}

fn link(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}
