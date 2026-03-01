//! Tests for SettleTlcSetCommand

use std::cell::RefCell;
use std::collections::HashMap;

use ckb_types::packed::{OutPoint, Script};
use tentacle::secio::PeerId;

use crate::fiber::channel::InboundTlcStatus;
use crate::fiber::channel::{
    AppliedFlags, ChannelActorState, ChannelActorStateStore, ChannelBasePublicKeys,
    ChannelConstraints, ChannelState, ChannelTlcInfo, CommitDiff, CommitmentNumbers,
    InMemorySigner, TLCId, TlcInfo, TlcState, TlcStatus,
};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::payment::PaymentCustomRecords;
use crate::fiber::settle_tlc_set_command::{SettleTlcSetCommand, TlcSettlement};
use crate::fiber::types::{Hash256, HoldTlc, RemoveTlcReason, TlcErrorCode, NO_SHARED_SECRET};
use crate::gen_rand_sha256_hash;
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceError};
use crate::invoice::{InvoiceStore, PreimageStore};
use crate::now_timestamp_as_millis_u64;
use crate::tests::gen_utils::gen_rand_fiber_public_key;
use crate::time::SystemTime;

/// Mock store for testing that implements PreimageStore, InvoiceStore, and ChannelActorStateStore
struct MockStore {
    invoices: RefCell<HashMap<Hash256, CkbInvoice>>,
    invoice_statuses: RefCell<HashMap<Hash256, CkbInvoiceStatus>>,
    preimages: RefCell<HashMap<Hash256, Hash256>>,
    hold_tlcs: RefCell<HashMap<Hash256, Vec<HoldTlc>>>,
    channel_states: RefCell<HashMap<Hash256, ChannelActorState>>,
}

impl MockStore {
    fn new() -> Self {
        Self {
            invoices: RefCell::new(HashMap::new()),
            invoice_statuses: RefCell::new(HashMap::new()),
            preimages: RefCell::new(HashMap::new()),
            hold_tlcs: RefCell::new(HashMap::new()),
            channel_states: RefCell::new(HashMap::new()),
        }
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

    fn with_preimage(self, payment_hash: Hash256, preimage: Hash256) -> Self {
        self.preimages.borrow_mut().insert(payment_hash, preimage);
        self
    }

    fn with_channel_state(self, state: ChannelActorState) -> Self {
        let channel_id = state.id;
        self.channel_states.borrow_mut().insert(channel_id, state);
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
        self.preimages.borrow_mut().remove(payment_hash);
    }

    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        self.preimages.borrow().get(payment_hash).cloned()
    }
}

impl ChannelActorStateStore for MockStore {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        self.channel_states.borrow().get(id).cloned()
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        let channel_id = state.id;
        self.channel_states.borrow_mut().insert(channel_id, state);
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        self.channel_states.borrow_mut().remove(id);
    }

    fn get_channel_ids_by_peer(&self, _peer_id: &PeerId) -> Vec<Hash256> {
        self.channel_states.borrow().keys().cloned().collect()
    }

    fn get_channel_states(&self, _peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)> {
        vec![]
    }

    fn get_channel_state_by_outpoint(&self, _id: &OutPoint) -> Option<ChannelActorState> {
        None
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

    fn is_tlc_settled(&self, _channel_id: &Hash256, _payment_hash: &Hash256) -> bool {
        false
    }

    fn store_pending_commit_diff(&self, _channel_id: &Hash256, _diff: &CommitDiff) {
        // No-op for tests
    }

    fn get_pending_commit_diff(&self, _channel_id: &Hash256) -> Option<CommitDiff> {
        None
    }

    fn delete_pending_commit_diff(&self, _channel_id: &Hash256) {
        // No-op for tests
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

fn create_test_channel_state_with_tlc(
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
        shared_secret: NO_SHARED_SECRET,
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
        signer,
        local_channel_public_keys: ChannelBasePublicKeys {
            funding_pubkey: gen_rand_fiber_public_key(),
            tlc_base_key: gen_rand_fiber_public_key(),
        },
        commitment_numbers: CommitmentNumbers::default(),
        local_constraints: ChannelConstraints::default(),
        remote_constraints: ChannelConstraints::default(),
        tlc_state,
        pending_replay_updates: vec![],
        retryable_tlc_operations: std::collections::VecDeque::new(),
        waiting_forward_tlc_tasks: HashMap::new(),
        remote_shutdown_script: None,
        local_shutdown_script: Script::default(),
        last_committed_remote_nonce: None,
        remote_revocation_nonce_for_verify: None,
        remote_revocation_nonce_for_send: None,
        remote_revocation_nonce_for_next: None,
        remote_commitment_points: vec![],
        remote_channel_public_keys: None,
        local_shutdown_info: None,
        remote_shutdown_info: None,
        shutdown_transaction_hash: None,
        latest_commitment_transaction: None,
        reestablishing: false,
        last_revoke_ack_msg: None,
        created_at: SystemTime::now(),
        waiting_peer_response: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: std::collections::VecDeque::new(),
        last_was_revoke: false,
        ephemeral_config: Default::default(),
        private_key: None,
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
            packet.decode(&[0u8; 32], vec![]).map(|e| e.error_code)
        }
        _ => None,
    }
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
fn test_received_invoice_proceeds_to_settlement() {
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

    // Use hold path (empty channel_tlc_ids) so Received is allowed (hold-invoice re-entry with preimage).
    let command = SettleTlcSetCommand::new(payment_hash, vec![], &store);
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

    // Use hold path so Received is allowed (hold-invoice re-entry; invoice expiry is ignored for Received).
    let command = SettleTlcSetCommand::new(payment_hash, vec![], &store);
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
