//! Tests for SettleTlcSetCommand

use crate::fiber::channel::{
    has_pending_tlc_for_payment_hash, ChannelActorState, ChannelActorStateStore,
};
use crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement;
use crate::fiber::settle_tlc_set_command::{
    SettleOnChainFulfilledInvoiceCommand, SettleTlcSetCommand, TlcSettlement,
};
use crate::fiber::types::{Hash256, HoldTlc, Pubkey, RemoveTlcReason};
use crate::gen_rand_sha256_hash;
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceError};
use crate::invoice::{InvoiceStore, PreimageStore};
use crate::now_timestamp_as_millis_u64;
use crate::tests::gen_utils::gen_rand_fiber_public_key;
use crate::time::SystemTime;
use ckb_types::packed::{OutPoint, Script};
use fiber_sphinx::OnionErrorPacket;
use fiber_types::{
    AppliedFlags, ChannelActorData, ChannelBasePublicKeys, ChannelState, ChannelTlcInfo,
    CommitmentNumbers, InMemorySigner, PaymentCustomRecords, ShutdownSettlementRecord, TLCId,
    TlcInfo, TlcState, TlcStatus,
};
use fiber_types::{ChannelConstraints, InboundTlcStatus};
use fiber_types::{HashAlgorithm, RemoveTlcFulfill, TlcErr, TlcErrorCode};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const TEST_SHARED_SECRET: [u8; 32] = [42u8; 32];

type ChannelStatesHook = Box<dyn FnOnce(&MockStore)>;

/// Mock store for testing that implements PreimageStore, InvoiceStore, and ChannelActorStateStore
pub(crate) struct MockStore {
    invoices: RefCell<HashMap<Hash256, CkbInvoice>>,
    invoice_statuses: RefCell<HashMap<Hash256, CkbInvoiceStatus>>,
    preimages: RefCell<HashMap<Hash256, Hash256>>,
    hold_tlcs: RefCell<HashMap<Hash256, Vec<HoldTlc>>>,
    channel_states: RefCell<HashMap<Hash256, ChannelActorState>>,
    pub(crate) channel_state_writes: Cell<usize>,
    pub(crate) preimage_removal_writes: RefCell<Vec<usize>>,
    pub(crate) onchain_settlements: RefCell<HashMap<(Hash256, TLCId), OnChainTlcSettlement>>,
    pub(crate) shutdown_settlements: RefCell<HashMap<Hash256, ShutdownSettlementRecord>>,
    pub(crate) watch_channels: RefCell<HashMap<Hash256, fiber_types::ChannelData>>,
    on_get_channel_states: RefCell<Option<ChannelStatesHook>>,
}

impl MockStore {
    /// Test doubles keep the whole state in memory, so seeding an already
    /// established channel needs no separate fixed-field record.
    pub(crate) fn insert_channel_actor_state_for_test(&self, state: ChannelActorState) {
        self.insert_channel_actor_state(state);
    }

    pub(crate) fn new() -> Self {
        Self {
            invoices: RefCell::new(HashMap::new()),
            invoice_statuses: RefCell::new(HashMap::new()),
            preimages: RefCell::new(HashMap::new()),
            hold_tlcs: RefCell::new(HashMap::new()),
            channel_states: RefCell::new(HashMap::new()),
            channel_state_writes: Cell::new(0),
            preimage_removal_writes: RefCell::new(Vec::new()),
            onchain_settlements: RefCell::new(HashMap::new()),
            shutdown_settlements: RefCell::new(HashMap::new()),
            watch_channels: RefCell::new(HashMap::new()),
            on_get_channel_states: RefCell::new(None),
        }
    }

    fn with_on_get_channel_states<F>(self, hook: F) -> Self
    where
        F: FnOnce(&MockStore) + 'static,
    {
        *self.on_get_channel_states.borrow_mut() = Some(Box::new(hook));
        self
    }

    fn with_hold_tlc(self, payment_hash: Hash256, channel_id: Hash256, tlc_id: u64) -> Self {
        self.hold_tlcs
            .borrow_mut()
            .entry(payment_hash)
            .or_default()
            .push(HoldTlc {
                channel_id,
                tlc_id,
                hold_expire_at: 0,
            });
        self
    }

    fn with_invoice(self, invoice: CkbInvoice, status: CkbInvoiceStatus) -> Self {
        let payment_hash = *invoice.payment_hash();
        self.invoices.borrow_mut().insert(payment_hash, invoice);
        self.invoice_statuses
            .borrow_mut()
            .insert(payment_hash, status);
        self
    }

    pub(crate) fn with_preimage(self, payment_hash: Hash256, preimage: Hash256) -> Self {
        self.preimages.borrow_mut().insert(payment_hash, preimage);
        self
    }

    fn with_channel_state(self, state: ChannelActorState) -> Self {
        let channel_id = state.id;
        self.channel_states.borrow_mut().insert(channel_id, state);
        self
    }

    pub(crate) fn with_onchain_settlement(
        self,
        channel_id: Hash256,
        tlc_id: TLCId,
        settlement: OnChainTlcSettlement,
    ) -> Self {
        self.onchain_settlements
            .borrow_mut()
            .insert((channel_id, tlc_id), settlement);
        self
    }

    pub(crate) fn with_onchain_settled(
        self,
        channel_id: Hash256,
        tlc_id: TLCId,
        payment_hash: Hash256,
        hash_algorithm: HashAlgorithm,
    ) -> Self {
        self.with_onchain_settlement(
            channel_id,
            tlc_id,
            OnChainTlcSettlement {
                payment_hash,
                hash_algorithm,
                preimage: None,
                tx_hash: gen_rand_sha256_hash(),
                tlc_index: 0,
            },
        )
    }

    pub(crate) fn with_onchain_preimage(
        self,
        channel_id: Hash256,
        tlc_id: TLCId,
        payment_hash: Hash256,
        hash_algorithm: HashAlgorithm,
        preimage: Hash256,
    ) -> Self {
        self.with_onchain_settlement(
            channel_id,
            tlc_id,
            OnChainTlcSettlement {
                payment_hash,
                hash_algorithm,
                preimage: Some(preimage),
                tx_hash: gen_rand_sha256_hash(),
                tlc_index: 0,
            },
        )
    }

    #[allow(dead_code)]
    pub(crate) fn with_shutdown_settlement(
        self,
        channel_id: Hash256,
        record: ShutdownSettlementRecord,
    ) -> Self {
        self.shutdown_settlements
            .borrow_mut()
            .insert(channel_id, record);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_watch_channel(self, channel: fiber_types::ChannelData) -> Self {
        self.watch_channels
            .borrow_mut()
            .insert(channel.channel_id, channel);
        self
    }
}

impl InvoiceStore for MockStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        self.invoices.borrow().get(id).cloned()
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        _preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let payment_hash = *invoice.payment_hash();
        self.invoices.borrow_mut().insert(payment_hash, invoice);
        self.invoice_statuses
            .borrow_mut()
            .insert(payment_hash, CkbInvoiceStatus::Open);
        Ok(())
    }

    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: CkbInvoiceStatus,
    ) -> Result<(), InvoiceError> {
        self.invoice_statuses.borrow_mut().insert(*id, status);
        Ok(())
    }

    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus> {
        self.invoice_statuses.borrow().get(id).cloned()
    }
}

impl PreimageStore for MockStore {
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        self.preimages.borrow_mut().insert(payment_hash, preimage);
    }

    fn remove_preimage(&self, payment_hash: &Hash256) {
        self.preimage_removal_writes
            .borrow_mut()
            .push(self.channel_state_writes.get());
        self.preimages.borrow_mut().remove(payment_hash);
    }

    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        self.preimages.borrow().get(payment_hash).cloned()
    }
}

impl ChannelActorStateStore for MockStore {
    fn has_trampoline_payment_for_tlc(
        &self,
        _payment_hash: Hash256,
        _channel_id: Hash256,
        _tlc_id: u64,
    ) -> bool {
        false
    }

    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        self.channel_states.borrow().get(id).cloned()
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        self.channel_state_writes
            .set(self.channel_state_writes.get() + 1);
        let channel_id = state.id;
        self.channel_states.borrow_mut().insert(channel_id, state);
    }

    fn move_channel_actor_state(&self, old_id: &Hash256, state: ChannelActorState) {
        self.channel_states.borrow_mut().remove(old_id);
        let channel_id = state.id;
        self.channel_states.borrow_mut().insert(channel_id, state);
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        self.channel_states.borrow_mut().remove(id);
    }

    fn get_channel_ids_by_pubkey(&self, _pubkey: &Pubkey) -> Vec<Hash256> {
        self.channel_states.borrow().keys().cloned().collect()
    }

    fn get_channel_states(&self, _pubkey: Option<Pubkey>) -> Vec<(Pubkey, Hash256, ChannelState)> {
        let hook = self.on_get_channel_states.borrow_mut().take();
        if let Some(hook) = hook {
            hook(self);
        }
        self.channel_states
            .borrow()
            .values()
            .map(|state| (state.remote_pubkey, state.id, state.state))
            .collect()
    }

    fn get_channel_state_by_outpoint(&self, _id: &OutPoint) -> Option<ChannelActorState> {
        None
    }

    fn get_all_channel_states(&self) -> Vec<ChannelActorState> {
        vec![]
    }

    fn insert_payment_custom_records(
        &self,
        _payment_hash: &Hash256,
        _custom_records: PaymentCustomRecords,
    ) {
        // No-op for tests
    }

    fn get_payment_custom_records(&self, _payment_hash: &Hash256) -> Option<PaymentCustomRecords> {
        None
    }

    fn insert_payment_hold_tlc(&self, payment_hash: Hash256, hold_tlc: HoldTlc) {
        self.hold_tlcs
            .borrow_mut()
            .entry(payment_hash)
            .or_default()
            .push(hold_tlc);
    }

    fn remove_payment_hold_tlc(&self, payment_hash: &Hash256, channel_id: &Hash256, tlc_id: u64) {
        self.hold_tlcs
            .borrow_mut()
            .entry(*payment_hash)
            .and_modify(|v| {
                v.retain(|h| h.channel_id != *channel_id || h.tlc_id != tlc_id);
            });
    }

    fn get_payment_hold_tlcs(&self, payment_hash: Hash256) -> Vec<HoldTlc> {
        self.hold_tlcs
            .borrow()
            .get(&payment_hash)
            .cloned()
            .unwrap_or_default()
    }

    fn get_node_hold_tlcs(&self) -> HashMap<Hash256, Vec<HoldTlc>> {
        HashMap::new()
    }

    fn get_onchain_tlc_settlement(
        &self,
        channel_id: &Hash256,
        tlc_id: TLCId,
        _payment_hash: &Hash256,
    ) -> Option<OnChainTlcSettlement> {
        self.onchain_settlements
            .borrow()
            .get(&(*channel_id, tlc_id))
            .cloned()
    }

    fn store_shutdown_settlement_record(
        &self,
        channel_id: &Hash256,
        record: &ShutdownSettlementRecord,
    ) {
        self.shutdown_settlements
            .borrow_mut()
            .insert(*channel_id, record.clone());
    }

    fn get_shutdown_settlement_record(
        &self,
        channel_id: &Hash256,
    ) -> Option<ShutdownSettlementRecord> {
        self.shutdown_settlements.borrow().get(channel_id).cloned()
    }

    fn delete_shutdown_settlement_record(&self, channel_id: &Hash256) {
        self.shutdown_settlements.borrow_mut().remove(channel_id);
    }

    fn get_local_watch_channel(&self, channel_id: &Hash256) -> Option<fiber_types::ChannelData> {
        self.watch_channels.borrow().get(channel_id).cloned()
    }
}

fn create_test_invoice(payment_hash: Hash256, amount: Option<u128>, allow_mpp: bool) -> CkbInvoice {
    let mut builder = InvoiceBuilder::new(Currency::Fibd)
        .payment_hash(payment_hash)
        .amount(amount);

    if allow_mpp {
        builder = builder
            .allow_mpp(true)
            .payment_secret(gen_rand_sha256_hash());
    }

    builder.build().expect("build invoice")
}

pub(crate) fn create_test_channel_state_with_tlc(
    channel_id: Hash256,
    tlc_id: u64,
    amount: u128,
    payment_hash: Hash256,
    total_amount: Option<u128>,
) -> ChannelActorState {
    let tlc_info = TlcInfo {
        status: TlcStatus::Inbound(InboundTlcStatus::Committed),
        tlc_id: TLCId::Received(tlc_id),
        amount,
        payment_hash,
        total_amount,
        payment_secret: None,
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 1000000,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: TEST_SHARED_SECRET,
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::empty(),
        forwarding_tlc: None,
    };

    let mut tlc_state = TlcState::default();
    tlc_state.received_tlcs.tlcs.push(tlc_info);
    tlc_state.received_tlcs.next_tlc_id = tlc_id + 1;

    let seed = [0u8; 32];
    let signer = InMemorySigner::generate_from_seed(&seed);

    ChannelActorState {
        core: ChannelActorData {
            state: ChannelState::ChannelReady,
            public_channel_info: None,
            local_tlc_info: ChannelTlcInfo::default(),
            remote_tlc_info: None,
            local_pubkey: gen_rand_fiber_public_key(),
            remote_pubkey: gen_rand_fiber_public_key(),
            id: channel_id,
            funding_tx: None,
            funding_tx_confirmed_at: None,
            funding_udt_type_script: None,
            is_acceptor: false,
            is_one_way: false,
            to_local_amount: 0,
            to_remote_amount: 0,
            local_reserved_ckb_amount: 0,
            remote_reserved_ckb_amount: 0,
            commitment_fee_rate: 0,
            commitment_delay_epoch: 0,
            funding_fee_rate: 0,
            session: crate::fiber::session::new_session(channel_id, &signer.funding_key, None),
            signer,
            local_channel_public_keys: ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            },
            commitment_numbers: CommitmentNumbers::default(),
            local_constraints: ChannelConstraints::default(),
            remote_constraints: ChannelConstraints::default(),
            tlc_state,
            retryable_tlc_operations: std::collections::VecDeque::new(),
            waiting_forward_tlc_tasks: HashMap::new(),
            remote_shutdown_script: None,
            local_shutdown_script: Script::default(),
            remote_commitment_points: vec![],
            remote_channel_public_keys: None,
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            reestablishing: false,
            connectivity_state: fiber_types::ChannelConnectivityState::Online,
            pending_replay_updates: vec![],
            last_was_revoke: false,
            external_funding: None,
            channel_features: Default::default(),

            created_at: SystemTime::now(),
        },
        waiting_peer_response: None,
        reestablish_timer: None,
        recovery_peer_v2: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        pending_remove_effects_v2: 0,
        pending_payment_notifications: Default::default(),
        replayed_tlc_updates_v2: 0,
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: std::collections::VecDeque::new(),
        ephemeral_config: Default::default(),
        funding_abort_detail: None,
        private_key: None,
        needs_backup: false,
        musig2_agg_context_local_first: Default::default(),
        musig2_agg_context_remote_first: Default::default(),
    }
}

fn is_fulfill_settlement(settlement: &TlcSettlement) -> bool {
    matches!(
        settlement.remove_tlc_command().reason,
        RemoveTlcReason::RemoveTlcFulfill(_)
    )
}

fn get_error_code(settlement: &TlcSettlement) -> Option<TlcErrorCode> {
    match &settlement.remove_tlc_command().reason {
        RemoveTlcReason::RemoveTlcFail(packet) => {
            let (_, payload) = OnionErrorPacket::from_bytes(packet.onion_packet.clone())
                .xor_cipher_stream(&TEST_SHARED_SECRET)
                .split();
            TlcErr::deserialize(&payload).map(|e| e.error_code)
        }
        _ => None,
    }
}

#[test]
fn test_preimage_cleanup_detects_pending_same_hash_tlc_in_other_channel() {
    let payment_hash = gen_rand_sha256_hash();
    let current_channel_id = gen_rand_sha256_hash();
    let other_channel_id = gen_rand_sha256_hash();
    let mut current_state =
        create_test_channel_state_with_tlc(current_channel_id, 0, 1000, payment_hash, None);
    current_state.tlc_state.apply_remove_tlc(TLCId::Received(0));
    let other_state =
        create_test_channel_state_with_tlc(other_channel_id, 0, 1000, payment_hash, None);
    let store = MockStore::new()
        .with_channel_state(current_state.clone())
        .with_channel_state(other_state);

    assert!(
        has_pending_tlc_for_payment_hash(&store, &current_state, payment_hash),
        "preimage should stay while another channel has a same-hash tlc"
    );
}

#[test]
fn test_preimage_cleanup_ignores_stale_current_channel_state() {
    let payment_hash = gen_rand_sha256_hash();
    let current_channel_id = gen_rand_sha256_hash();
    let persisted_current_state =
        create_test_channel_state_with_tlc(current_channel_id, 0, 1000, payment_hash, None);
    let mut current_state = persisted_current_state.clone();
    current_state.tlc_state.apply_remove_tlc(TLCId::Received(0));
    let store = MockStore::new().with_channel_state(persisted_current_state);

    assert!(
        !has_pending_tlc_for_payment_hash(&store, &current_state, payment_hash),
        "stale persisted state for the channel currently applying removal must not keep preimage"
    );
}

#[test]
fn test_invoice_settlement_skips_pending_forwarded_tlcs() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let waiting_channel_id = gen_rand_sha256_hash();
    let forwarded_channel_id = gen_rand_sha256_hash();
    let mut waiting =
        create_test_channel_state_with_tlc(waiting_channel_id, 0, 1000, payment_hash, None);
    waiting
        .waiting_forward_tlc_tasks
        .insert(TLCId::Received(0), TEST_SHARED_SECRET);
    let mut forwarded =
        create_test_channel_state_with_tlc(forwarded_channel_id, 0, 1000, payment_hash, None);
    forwarded.tlc_state.received_tlcs.tlcs[0].forwarding_tlc = Some((gen_rand_sha256_hash(), 0));
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Open,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(waiting)
        .with_channel_state(forwarded);

    let settlements = SettleTlcSetCommand::new(
        payment_hash,
        vec![(waiting_channel_id, 0), (forwarded_channel_id, 0)],
        &store,
    )
    .run();

    assert!(settlements.is_empty());
    assert_eq!(
        store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Open)
    );
}

#[test]
fn test_onchain_forwarded_tlc_does_not_pay_local_invoice() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let channel_id = gen_rand_sha256_hash();
    let mut state = create_test_channel_state_with_tlc(channel_id, 0, 1000, payment_hash, None);
    let tlc = &mut state.tlc_state.received_tlcs.tlcs[0];
    tlc.forwarding_tlc = Some((gen_rand_sha256_hash(), 0));
    tlc.removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Open,
        )
        .with_channel_state(state)
        .with_onchain_preimage(
            channel_id,
            TLCId::Received(0),
            payment_hash,
            HashAlgorithm::CkbHash,
            preimage,
        );

    assert!(!SettleOnChainFulfilledInvoiceCommand::new(payment_hash, &store).run());
    assert_eq!(
        store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Open)
    );
}

#[test]
fn test_no_invoice_rejects_all_tlcs() {
    let payment_hash = gen_rand_sha256_hash();
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();

    let store = MockStore::new()
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            None,
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            2000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 2);
    for settlement in &settlements {
        assert!(!is_fulfill_settlement(settlement));
        assert_eq!(
            get_error_code(settlement),
            Some(TlcErrorCode::InvoiceCancelled)
        );
    }
}

#[test]
fn test_invoice_without_status_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();

    // Only insert invoice but not status
    let store = MockStore::new().with_channel_state(create_test_channel_state_with_tlc(
        channel_id,
        0,
        1000,
        payment_hash,
        None,
    ));
    store.invoices.borrow_mut().insert(payment_hash, invoice);

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::InvoiceCancelled)
    );
}

#[test]
fn test_expired_invoice_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Expired)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::InvoiceExpired)
    );
}

#[test]
fn test_cancelled_invoice_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Cancelled)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::InvoiceCancelled)
    );
}

#[test]
fn test_paid_invoice_rejects_with_timeout() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Paid)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::HoldTlcTimeout)
    );
}

#[test]
fn test_open_invoice_proceeds_to_settlement() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_duplicate_received_hold_set_is_noop() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Received)
        .with_hold_tlc(payment_hash, channel_id, 0)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let command = SettleTlcSetCommand::new_hold_tlc_set(payment_hash, &store);
    let settlements = command.run();

    assert!(settlements.is_empty());
    assert_eq!(store.get_payment_hold_tlcs(payment_hash).len(), 1);
}

#[test]
fn test_received_hold_invoice_after_preimage_reveal_proceeds_to_settlement() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Received)
        .with_preimage(payment_hash, preimage)
        .with_hold_tlc(payment_hash, channel_id, 0)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let command = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_non_mpp_insufficient_amount_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            500,
            payment_hash,
            None,
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            400,
            payment_hash,
            None,
        ));

    // All TLCs have insufficient amount
    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 2);
    for settlement in &settlements {
        assert_eq!(
            get_error_code(settlement),
            Some(TlcErrorCode::IncorrectOrUnknownPaymentDetails)
        );
    }
}

#[test]
fn test_non_mpp_single_fulfilling_tlc_settles() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_non_mpp_overpaying_tlc_settles() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            2000,
            payment_hash,
            None,
        )); // Overpaying

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_non_mpp_multiple_tlcs_keeps_first_fulfilling_rejects_rest() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let channel_id_2 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            500,
            payment_hash,
            None,
        )) // Not enough
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            None,
        )) // Enough - should be kept
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_2,
            2,
            1500,
            payment_hash,
            None,
        )); // Also enough but rejected

    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1), (channel_id_2, 2)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 3);

    // Find the fulfilled settlement (should be from channel_id_1)
    let fulfilled: Vec<_> = settlements
        .iter()
        .filter(|s| is_fulfill_settlement(s))
        .collect();
    let rejected: Vec<_> = settlements
        .iter()
        .filter(|s| !is_fulfill_settlement(s))
        .collect();

    assert_eq!(fulfilled.len(), 1);
    assert_eq!(rejected.len(), 2);

    // The fulfilled TLC should be from channel_id_1 (first one meeting the threshold)
    assert_eq!(fulfilled[0].channel_id(), channel_id_1);

    // Rejected TLCs should have HoldTlcTimeout error
    for settlement in rejected {
        assert_eq!(
            get_error_code(settlement),
            Some(TlcErrorCode::HoldTlcTimeout)
        );
    }
}

#[test]
fn test_mpp_inconsistent_total_amount_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(3000), true);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let channel_id_2 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            Some(4000),
        )) // Different!
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_2,
            2,
            1000,
            payment_hash,
            Some(3000),
        ));

    // TLCs with inconsistent total_amount
    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1), (channel_id_2, 2)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 3);
    for settlement in &settlements {
        assert_eq!(
            get_error_code(settlement),
            Some(TlcErrorCode::IncorrectOrUnknownPaymentDetails)
        );
    }
}

#[test]
fn test_mpp_total_amount_below_invoice_rejects_all() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(5000), true);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            Some(3000),
        ));

    // total_amount is less than invoice amount
    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 2);
    for settlement in &settlements {
        assert_eq!(
            get_error_code(settlement),
            Some(TlcErrorCode::IncorrectOrUnknownPaymentDetails)
        );
    }
}

#[test]
fn test_mpp_not_enough_accumulated_returns_empty() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(3000), true);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            Some(3000),
        ));

    // Only 1000 accumulated, need 3000
    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    // Not enough to fulfill, should return empty (waiting for more TLCs)
    assert!(settlements.is_empty());
}

#[test]
fn test_mpp_accumulated_fulfills_settles_all() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(3000), true);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let channel_id_2 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_2,
            2,
            1000,
            payment_hash,
            Some(3000),
        ));

    // Exactly enough: 1000 + 1000 + 1000 = 3000
    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1), (channel_id_2, 2)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 3);
    for settlement in &settlements {
        assert!(is_fulfill_settlement(settlement));
    }
}

#[test]
fn test_mpp_overpaid_tlcs_rejected() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(3000), true);
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let channel_id_2 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            2000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            2000,
            payment_hash,
            Some(3000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_2,
            2,
            2000,
            payment_hash,
            Some(3000),
        )); // Overpaid

    // More than enough: 2000 + 2000 + 2000 = 6000 (need 3000)
    // Should keep first two and reject third
    let channel_tlc_ids = vec![(channel_id_0, 0), (channel_id_1, 1), (channel_id_2, 2)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 3);

    let fulfilled: Vec<_> = settlements
        .iter()
        .filter(|s| is_fulfill_settlement(s))
        .collect();
    let rejected: Vec<_> = settlements
        .iter()
        .filter(|s| !is_fulfill_settlement(s))
        .collect();

    assert_eq!(fulfilled.len(), 2);
    assert_eq!(rejected.len(), 1);

    // Overpaid TLC should be rejected
    assert_eq!(rejected[0].channel_id(), channel_id_2);
    assert_eq!(
        get_error_code(rejected[0]),
        Some(TlcErrorCode::HoldTlcTimeout)
    );
}

#[test]
fn test_mpp_single_tlc_consistent_total_amount_check_passes() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), true);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            Some(1000),
        ));

    // Single TLC for MPP invoice - should not fail consistency check
    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_no_preimage_skips_settlement() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));
    // Note: no preimage added

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    // Should skip settlement and return empty
    assert!(settlements.is_empty());
}

#[test]
fn test_open_invoice_marked_as_received() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let _settlements = command.run();

    // Check that invoice status was updated to Received
    assert_eq!(
        store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Received)
    );
}

#[test]
fn test_received_invoice_not_updated() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Received)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let _settlements = command.run();

    // Status should remain Received
    assert_eq!(
        store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Received)
    );
}

#[test]
fn test_received_invoice_can_be_settled_after_invoice_expiry() {
    // When an invoice is in Received status, it should still be settleable
    // even after the invoice expiry time has passed. This is because the TLCs
    // have already arrived and are held - we should still allow settlement
    // as long as the TLCs themselves haven't expired.

    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));

    // Create invoice with 1 second expiry
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .payment_hash(payment_hash)
        .amount(Some(1000))
        .expiry_time(std::time::Duration::from_secs(1))
        .build()
        .expect("build invoice");

    // Wait for invoice to expire
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(invoice.is_expired(), "Invoice should be expired");

    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Received)
        .with_preimage(payment_hash, preimage)
        .with_hold_tlc(payment_hash, channel_id, 0)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    // Use preimage-reveal retry path so Received is allowed and invoice expiry is ignored.
    let command = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store);
    let settlements = command.run();

    // Should still succeed with fulfill (not fail)
    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_open_invoice_with_expired_expiry_rejects_tlcs() {
    // When an invoice is still in Open status but its expiry time has passed,
    // all TLCs should be rejected with InvoiceExpired error.

    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));

    // Create invoice with 1 second expiry
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .payment_hash(payment_hash)
        .amount(Some(1000))
        .expiry_time(std::time::Duration::from_secs(1))
        .build()
        .expect("build invoice");

    // Wait for invoice to expire
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(invoice.is_expired(), "Invoice should be expired");

    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open) // Still Open, not Received
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    // Should reject with InvoiceExpired
    assert_eq!(settlements.len(), 1);
    assert!(!is_fulfill_settlement(&settlements[0]));
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::InvoiceExpired)
    );
}

#[test]
fn test_empty_tlcs_returns_empty() {
    let payment_hash = gen_rand_sha256_hash();
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let store = MockStore::new().with_invoice(invoice, CkbInvoiceStatus::Open);

    let channel_tlc_ids: Vec<(Hash256, u64)> = vec![];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert!(settlements.is_empty());
}

#[test]
fn test_invoice_without_amount_accepts_any() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, None, false); // No amount
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1,
            payment_hash,
            None,
        )); // Any amount should be accepted

    let channel_tlc_ids = vec![(channel_id, 0)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
}

#[test]
fn test_tlc_settlement_accessors() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let tlc_id = 42u64;
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            tlc_id,
            1000,
            payment_hash,
            None,
        ));

    let channel_tlc_ids = vec![(channel_id, tlc_id)];

    let command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &store);
    let settlements = command.run();

    assert_eq!(settlements.len(), 1);
    let settlement = &settlements[0];

    assert_eq!(settlement.channel_id(), channel_id);
    assert_eq!(settlement.tlc_id(), tlc_id);
    assert!(matches!(
        settlement.remove_tlc_command().reason,
        RemoveTlcReason::RemoveTlcFulfill(_)
    ));
}

/// Runs `f` and counts the ERROR events it emits on this thread.
fn count_error_events<T>(f: impl FnOnce() -> T) -> (T, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

    struct ErrorCounter(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorCounter {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::ERROR {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    let errors = Arc::new(AtomicUsize::new(0));
    let counter =
        tracing::Dispatch::new(tracing_subscriber::registry().with(ErrorCounter(errors.clone())));
    // tracing caches a callsite's interest when the callsite is first hit. While only one
    // dispatcher is alive, it asks only the default of the thread that hits it, which can be
    // another test's thread with no subscriber. A second live dispatcher makes it ask all.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let result = tracing::dispatcher::with_default(&counter, f);
    (result, errors.load(Ordering::SeqCst))
}

#[test]
fn test_hold_invoice_payment_logs_no_error_when_nothing_is_rejected() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = create_test_invoice(payment_hash, Some(1000), false);
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(invoice, CkbInvoiceStatus::Open)
        .with_hold_tlc(payment_hash, channel_id, 0)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));

    // The TLC is held: the invoice becomes Received and nothing is settled yet.
    let (settlements, held_errors) =
        count_error_events(|| SettleTlcSetCommand::new_hold_tlc_set(payment_hash, &store).run());
    assert!(settlements.is_empty());
    assert_eq!(
        store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Received)
    );

    // The preimage is revealed: the held TLC is fulfilled.
    store.insert_preimage(payment_hash, preimage);
    let (settlements, settled_errors) = count_error_events(|| {
        SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run()
    });
    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));

    assert_eq!(
        [held_errors, settled_errors],
        [0, 0],
        "ERROR events when holding and when fulfilling the only TLC"
    );
}

#[test]
fn test_settlement_logs_no_error_when_nothing_is_rejected() {
    // Non-MPP, one TLC that pays the invoice.
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Open,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));
    let (settlements, non_mpp_errors) = count_error_events(|| {
        SettleTlcSetCommand::new(payment_hash, vec![(channel_id, 0)], &store).run()
    });
    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));

    // MPP, two parts that pay the invoice exactly.
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(2000), true),
            CkbInvoiceStatus::Open,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            Some(2000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            Some(2000),
        ));
    let (settlements, mpp_errors) = count_error_events(|| {
        SettleTlcSetCommand::new(
            payment_hash,
            vec![(channel_id_0, 0), (channel_id_1, 1)],
            &store,
        )
        .run()
    });
    assert_eq!(settlements.len(), 2);
    assert!(settlements.iter().all(is_fulfill_settlement));

    // A hold set with no TLCs left in the store.
    let payment_hash = gen_rand_sha256_hash();
    let store = MockStore::new().with_invoice(
        create_test_invoice(payment_hash, Some(1000), false),
        CkbInvoiceStatus::Open,
    );
    let (settlements, empty_set_errors) =
        count_error_events(|| SettleTlcSetCommand::new_hold_tlc_set(payment_hash, &store).run());
    assert!(settlements.is_empty());

    assert_eq!(
        [non_mpp_errors, mpp_errors, empty_set_errors],
        [0, 0, 0],
        "ERROR events for a non-MPP payment, an MPP payment and an empty TLC set"
    );
}

#[test]
fn test_rejected_tlcs_are_still_logged_as_error() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let channel_id_0 = gen_rand_sha256_hash();
    let channel_id_1 = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Open,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_0,
            0,
            1000,
            payment_hash,
            None,
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id_1,
            1,
            1000,
            payment_hash,
            None,
        ));

    // Two TLCs for a non-MPP invoice: one is fulfilled, the other rejected.
    let (settlements, extra_tlc_errors) = count_error_events(|| {
        SettleTlcSetCommand::new(
            payment_hash,
            vec![(channel_id_0, 0), (channel_id_1, 1)],
            &store,
        )
        .run()
    });
    assert_eq!(settlements.len(), 2);
    assert_eq!(
        settlements
            .iter()
            .filter(|s| is_fulfill_settlement(s))
            .count(),
        1
    );

    // A paid invoice rejects a late TLC.
    let payment_hash = gen_rand_sha256_hash();
    let channel_id = gen_rand_sha256_hash();
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Paid,
        )
        .with_channel_state(create_test_channel_state_with_tlc(
            channel_id,
            0,
            1000,
            payment_hash,
            None,
        ));
    let (settlements, paid_invoice_errors) = count_error_events(|| {
        SettleTlcSetCommand::new(payment_hash, vec![(channel_id, 0)], &store).run()
    });
    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::HoldTlcTimeout)
    );

    assert_eq!(
        [extra_tlc_errors, paid_invoice_errors],
        [1, 1],
        "ERROR events for a rejected extra TLC and a TLC on a paid invoice"
    );
}

#[test]
fn test_non_mpp_received_rejects_extra_tlc_when_already_fulfilling() {
    use fiber_types::{RemoveTlcFulfill, RemoveTlcReason};

    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    // Ensure ch_a < ch_b in channel_id order so ch_a appears first in payment_hold_tlcs.
    let ch_a = Hash256::from([1u8; 32]);
    let ch_b = Hash256::from([2u8; 32]);

    let mut state_b = create_test_channel_state_with_tlc(ch_b, 0, 1000, payment_hash, None);
    state_b
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));

    // Case A: ch_b already removed from payment_hold_tlcs (after network.rs:4509), ch_a remains.
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            1000,
            payment_hash,
            None,
        ))
        .with_channel_state(state_b)
        .with_hold_tlc(payment_hash, ch_a, 0);

    let settlements = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run();
    assert_eq!(settlements.len(), 1);
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::HoldTlcTimeout),
        "overpaid non-MPP TLC ch_a must be rejected when ch_b is already fulfilling"
    );
}

#[test]
fn test_mpp_received_rejects_extra_tlc_when_already_fulfilling() {
    use std::collections::HashSet;

    use fiber_types::{RemoveTlcFulfill, RemoveTlcReason};

    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let ch_a = Hash256::from([1u8; 32]);
    let ch_b = Hash256::from([2u8; 32]);
    let ch_c = Hash256::from([3u8; 32]);

    // Case 1: ch_b (600) + ch_c (400) are already fulfilling in store (even while still in
    // payment_hold_tlcs); overpaid ch_a (500, ch_a < ch_b) must be rejected, never fulfilled.
    let mut state_b = create_test_channel_state_with_tlc(ch_b, 0, 600, payment_hash, Some(1000));
    state_b
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));
    let mut state_c = create_test_channel_state_with_tlc(ch_c, 0, 400, payment_hash, Some(1000));
    state_c
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));

    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), true),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            500,
            payment_hash,
            Some(1000),
        ))
        .with_channel_state(state_b)
        .with_channel_state(state_c)
        .with_hold_tlc(payment_hash, ch_a, 0)
        .with_hold_tlc(payment_hash, ch_b, 0)
        .with_hold_tlc(payment_hash, ch_c, 0);

    let settlements = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run();
    assert!(
        settlements.iter().all(|s| !is_fulfill_settlement(s)),
        "no overpaid TLC should be fulfilled when invoice is already covered"
    );
    let rejected_channels: Vec<Hash256> = settlements
        .iter()
        .filter(|s| get_error_code(s) == Some(TlcErrorCode::HoldTlcTimeout))
        .map(|s| s.channel_id())
        .collect();
    assert_eq!(rejected_channels, vec![ch_a]);

    // Case 2: ch_b (600) + ch_c (400) are only in-flight in `pending_remove_tlcs` (Fulfill)
    // before the channel actor writes to store; overpaid ch_a (ch_a < ch_b) must still be
    // rejected, never fulfilled.
    let store_inflight = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), true),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            500,
            payment_hash,
            Some(1000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_b,
            0,
            600,
            payment_hash,
            Some(1000),
        ))
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_c,
            0,
            400,
            payment_hash,
            Some(1000),
        ))
        .with_hold_tlc(payment_hash, ch_a, 0)
        .with_hold_tlc(payment_hash, ch_b, 0)
        .with_hold_tlc(payment_hash, ch_c, 0);
    let inflight_fulfills = HashSet::from([(ch_b, 0), (ch_c, 0)]);
    let settlements_inflight = SettleTlcSetCommand::new_hold_tlc_set_with_in_flight(
        payment_hash,
        &store_inflight,
        true,
        &HashSet::new(),
        &inflight_fulfills,
    )
    .run();
    assert_eq!(settlements_inflight.len(), 1);
    assert_eq!(settlements_inflight[0].channel_id(), ch_a);
    assert_eq!(
        get_error_code(&settlements_inflight[0]),
        Some(TlcErrorCode::HoldTlcTimeout)
    );

    // Case 3: Amount-less MPP invoice with ch_b (1000, total_amount=1000) already fulfilling;
    // late ch_a declaring a larger total_amount=2000 must be rejected.
    let mut state_b_amountless =
        create_test_channel_state_with_tlc(ch_b, 0, 1000, payment_hash, Some(1000));
    state_b_amountless
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));
    let store_amountless = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, None, true),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            1000,
            payment_hash,
            Some(2000),
        ))
        .with_channel_state(state_b_amountless)
        .with_hold_tlc(payment_hash, ch_a, 0);
    let settlements_amountless =
        SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store_amountless).run();
    assert_eq!(settlements_amountless.len(), 1);
    assert_eq!(
        get_error_code(&settlements_amountless[0]),
        Some(TlcErrorCode::IncorrectOrUnknownPaymentDetails)
    );
}

#[test]
fn test_mpp_received_completes_remaining_part_after_hold_removed() {
    use fiber_types::{RemoveTlcFulfill, RemoveTlcReason};

    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let ch_0 = Hash256::from([1u8; 32]);
    let ch_1 = Hash256::from([2u8; 32]);

    let mut state_0 = create_test_channel_state_with_tlc(ch_0, 0, 600, payment_hash, Some(1000));
    state_0
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));

    // ch_0 (600) is already fulfilling and removed from payment_hold_tlcs; ch_1 (400) remains.
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), true),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(state_0)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_1,
            0,
            400,
            payment_hash,
            Some(1000),
        ))
        .with_hold_tlc(payment_hash, ch_1, 0);

    let settlements = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run();
    assert_eq!(settlements.len(), 1);
    assert!(is_fulfill_settlement(&settlements[0]));
    assert_eq!(settlements[0].channel_id(), ch_1);
}

#[test]
fn test_non_mpp_received_rejects_extra_tlc_when_winner_retires_concurrently() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let ch_a = Hash256::from([1u8; 32]);
    let ch_b = Hash256::from([2u8; 32]);

    let mut state_b = create_test_channel_state_with_tlc(ch_b, 0, 1000, payment_hash, None);
    state_b
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));

    // Simulate `ChannelActor(ch_b)` completing the winning TLC after `SettleTlcSetCommand::run()`
    // read `Received` and before `already_fulfilled_received_tlcs()` scans channels: it commits
    // `Paid` and retires the fulfilled TLC from `ch_b`, while leaving the preimage intact because
    // `ch_a` still holds an unfulfilled TLC for the same payment hash.
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), false),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            1000,
            payment_hash,
            None,
        ))
        .with_channel_state(state_b)
        .with_hold_tlc(payment_hash, ch_a, 0)
        .with_on_get_channel_states(move |store| {
            store
                .update_invoice_status(&payment_hash, CkbInvoiceStatus::Paid)
                .unwrap();
            let mut retired_b = store.get_channel_actor_state(&ch_b).unwrap();
            retired_b.tlc_state.apply_remove_tlc(TLCId::Received(0));
            store.insert_channel_actor_state(retired_b);
        });

    let settlements = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run();
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].channel_id(), ch_a);
    assert!(!is_fulfill_settlement(&settlements[0]));
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::HoldTlcTimeout),
        "extra non-MPP TLC must be rejected once winner commits Paid and retires"
    );
}

#[test]
fn test_mpp_received_rejects_extra_tlc_when_fulfilled_part_retires_concurrently() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let ch_a = Hash256::from([1u8; 32]);
    let ch_b = Hash256::from([2u8; 32]);
    let ch_c = Hash256::from([3u8; 32]);

    let mut state_b = create_test_channel_state_with_tlc(ch_b, 0, 600, payment_hash, Some(1000));
    state_b
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));
    let mut state_c = create_test_channel_state_with_tlc(ch_c, 0, 400, payment_hash, Some(1000));
    state_c
        .tlc_state
        .received_tlcs
        .tlcs
        .iter_mut()
        .find(|t| t.tlc_id == TLCId::Received(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));

    // Simulate `ch_b` (600) retiring concurrently with `Paid` after `SettleTlcSetCommand::run()`
    // read `Received`: `already_fulfilled_received_tlcs()` sees only `ch_c` (400 < 1000), so
    // without re-checking invoice status it would fulfill the extra `ch_a` (600) shard.
    let store = MockStore::new()
        .with_invoice(
            create_test_invoice(payment_hash, Some(1000), true),
            CkbInvoiceStatus::Received,
        )
        .with_preimage(payment_hash, preimage)
        .with_channel_state(create_test_channel_state_with_tlc(
            ch_a,
            0,
            600,
            payment_hash,
            Some(1000),
        ))
        .with_channel_state(state_b)
        .with_channel_state(state_c)
        .with_hold_tlc(payment_hash, ch_a, 0)
        .with_on_get_channel_states(move |store| {
            store
                .update_invoice_status(&payment_hash, CkbInvoiceStatus::Paid)
                .unwrap();
            let mut retired_b = store.get_channel_actor_state(&ch_b).unwrap();
            retired_b.tlc_state.apply_remove_tlc(TLCId::Received(0));
            store.insert_channel_actor_state(retired_b);
        });

    let settlements = SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &store).run();
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].channel_id(), ch_a);
    assert!(!is_fulfill_settlement(&settlements[0]));
    assert_eq!(
        get_error_code(&settlements[0]),
        Some(TlcErrorCode::HoldTlcTimeout),
        "extra MPP TLC must be rejected once a fulfilled shard commits Paid and retires"
    );
}
