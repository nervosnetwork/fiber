use crate::fiber::channel::{
    ForwardingHistoryCursor, ForwardingHistoryQuery, PaymentEventStore, PaymentHistoryCursor,
    PaymentHistoryQuery,
};
use crate::gen_rand_sha256_hash;
use crate::now_timestamp_as_millis_u64;
use crate::rpc::info::{
    fee_report_impl, forwarding_history_impl, payment_history_impl, received_payment_report_impl,
    sent_payment_report_impl, FeeReportParams, ForwardingHistoryAsset, ForwardingHistoryParams,
    PaymentHistoryAsset, PaymentHistoryEventType, PaymentHistoryParams, MILLIS_PER_DAY,
};
use ckb_types::packed::Script;
use fiber_types::{ForwardingEvent, PaymentEvent, PaymentEventType};
use std::sync::Mutex;

/// A simple in-memory mock store for testing the fee RPC logic
/// without requiring RocksDB.
///
/// Events are stored sorted by (timestamp, payment_hash) to match the
/// real store's key ordering.  The cursor is the composite key bytes
/// [timestamp_be_u64 ++ payment_hash(32)] of the last returned event.
struct MockForwardingStore {
    events: Mutex<Vec<ForwardingEvent>>,
    payment_events: Mutex<Vec<PaymentEvent>>,
}

impl MockForwardingStore {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            payment_events: Mutex::new(Vec::new()),
        }
    }
}

/// Build a cursor key for a forwarding event (mirrors the real key layout
/// used by the RocksDB store: prefix(1) + timestamp_be(8) + payment_hash(32)
/// + incoming_channel_id(32) = 73 bytes).
fn forwarding_cursor_key(event: &ForwardingEvent) -> Vec<u8> {
    let mut key = vec![240u8]; // FORWARDING_EVENT_PREFIX
    key.extend_from_slice(&event.timestamp.to_be_bytes());
    key.extend_from_slice(event.payment_hash.as_ref());
    key.extend_from_slice(event.incoming_channel_id.as_ref());
    key
}

fn forwarding_cursor_key_from_typed(cursor: &ForwardingHistoryCursor) -> Vec<u8> {
    let mut key = vec![240u8]; // FORWARDING_EVENT_PREFIX
    key.extend_from_slice(&cursor.timestamp().to_be_bytes());
    key.extend_from_slice(cursor.payment_hash().as_ref());
    key.extend_from_slice(cursor.incoming_channel_id().as_ref());
    key
}

/// Build a cursor key for a payment event (mirrors the real key layout:
/// prefix(1) + timestamp_be(8) + payment_hash(32) + channel_id(32) = 73 bytes).
fn payment_cursor_key(event: &PaymentEvent) -> Vec<u8> {
    let mut key = vec![241u8]; // PAYMENT_EVENT_PREFIX
    key.extend_from_slice(&event.timestamp.to_be_bytes());
    key.extend_from_slice(event.payment_hash.as_ref());
    key.extend_from_slice(event.channel_id.as_ref());
    key
}

fn payment_cursor_key_from_typed(cursor: &PaymentHistoryCursor) -> Vec<u8> {
    let mut key = vec![241u8]; // PAYMENT_EVENT_PREFIX
    key.extend_from_slice(&cursor.timestamp().to_be_bytes());
    key.extend_from_slice(cursor.payment_hash().as_ref());
    key.extend_from_slice(cursor.channel_id().as_ref());
    key
}

impl PaymentEventStore for MockForwardingStore {
    fn insert_forwarding_event(&self, event: ForwardingEvent) {
        let mut events = self.events.lock().unwrap();
        events.push(event);
        // Keep sorted by (timestamp, payment_hash, incoming_channel_id) to match real store
        events.sort_by_key(forwarding_cursor_key);
    }

    fn query_forwarding_events(
        &self,
        query: ForwardingHistoryQuery,
    ) -> Result<(Vec<ForwardingEvent>, Option<ForwardingHistoryCursor>), String> {
        let ForwardingHistoryQuery {
            asset,
            start_time,
            end_time,
            limit,
            after,
        } = query;

        if let Some(cursor) = &after {
            cursor.validate_for(&asset)?;
        }

        let filtered: Vec<ForwardingEvent> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.timestamp >= start_time && e.timestamp <= end_time)
            .filter(|e| asset.matches(&e.udt_type_script))
            .filter(|e| {
                if let Some(ref cursor) = after {
                    forwarding_cursor_key(e) > forwarding_cursor_key_from_typed(cursor)
                } else {
                    true
                }
            })
            .take(limit)
            .cloned()
            .collect();

        let last_cursor = if filtered.len() == limit {
            filtered
                .last()
                .map(|event| ForwardingHistoryCursor::new(&asset, event))
        } else {
            None
        };

        Ok((filtered, last_cursor))
    }

    fn insert_payment_event(&self, event: PaymentEvent) {
        let mut events = self.payment_events.lock().unwrap();
        events.push(event);
        // Keep sorted by (timestamp, payment_hash, channel_id) to match real store
        events.sort_by_key(payment_cursor_key);
    }

    fn query_payment_events(
        &self,
        query: PaymentHistoryQuery,
    ) -> Result<(Vec<PaymentEvent>, Option<PaymentHistoryCursor>), String> {
        if let Some(cursor) = &query.after {
            cursor.validate_for(&query)?;
        }

        let filtered: Vec<PaymentEvent> = self
            .payment_events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.timestamp >= query.start_time && e.timestamp <= query.end_time)
            .filter(|e| query.asset.matches(&e.udt_type_script))
            .filter(|e| {
                query
                    .event_type
                    .map(|expected| expected == e.event_type)
                    .unwrap_or(true)
            })
            .filter(|e| {
                if let Some(ref cursor) = query.after {
                    payment_cursor_key(e) > payment_cursor_key_from_typed(cursor)
                } else {
                    true
                }
            })
            .take(query.limit)
            .cloned()
            .collect();

        let last_cursor = if filtered.len() == query.limit {
            filtered
                .last()
                .map(|event| PaymentHistoryCursor::new(&query, event))
        } else {
            None
        };

        Ok((filtered, last_cursor))
    }
}

fn make_event(timestamp: u64, fee: u128) -> ForwardingEvent {
    make_event_with_udt(timestamp, fee, None)
}

fn make_event_with_udt(
    timestamp: u64,
    fee: u128,
    udt_type_script: Option<Script>,
) -> ForwardingEvent {
    ForwardingEvent {
        timestamp,
        incoming_channel_id: gen_rand_sha256_hash(),
        outgoing_channel_id: gen_rand_sha256_hash(),
        incoming_amount: 1000 + fee,
        outgoing_amount: 1000,
        fee,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script,
    }
}

/// Build a dummy UDT type script for testing.
fn dummy_udt_script(tag: u8) -> Script {
    use ckb_types::packed::ScriptBuilder;
    use ckb_types::prelude::*;
    ScriptBuilder::default()
        .code_hash(ckb_types::packed::Byte32::new([tag; 32]))
        .hash_type(ckb_types::core::ScriptHashType::Data)
        .build()
}

#[tokio::test]
async fn test_fee_report_empty() {
    let store = MockForwardingStore::new();
    let report = fee_report_impl(&store, FeeReportParams::default()).unwrap();

    assert!(report.asset_reports.is_empty());
}

#[tokio::test]
async fn test_fee_report_ckb_only() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Event within last day (CKB)
    store.insert_forwarding_event(make_event(now - MILLIS_PER_DAY / 2, 10));
    // Event within last week but not last day (CKB)
    store.insert_forwarding_event(make_event(now - 3 * MILLIS_PER_DAY, 20));
    // Event within last month but not last week (CKB)
    store.insert_forwarding_event(make_event(now - 15 * MILLIS_PER_DAY, 30));

    let report = fee_report_impl(&store, FeeReportParams::default()).unwrap();

    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert!(ckb.udt_type_script.is_none());
    assert_eq!(ckb.daily_fee_sum, 10);
    assert_eq!(ckb.daily_event_count, 1);
    assert_eq!(ckb.weekly_fee_sum, 30); // day + week
    assert_eq!(ckb.weekly_event_count, 2);
    assert_eq!(ckb.monthly_fee_sum, 60); // day + week + month
    assert_eq!(ckb.monthly_event_count, 3);
}

#[tokio::test]
async fn test_fee_report_multiple_assets() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt_a = dummy_udt_script(1);

    // CKB events
    store.insert_forwarding_event(make_event(now - 1000, 10));
    store.insert_forwarding_event(make_event(now - 2000, 20));

    // UDT-A events
    store.insert_forwarding_event(make_event_with_udt(now - 1000, 100, Some(udt_a.clone())));
    store.insert_forwarding_event(make_event_with_udt(now - 2000, 200, Some(udt_a.clone())));

    let report = fee_report_impl(&store, FeeReportParams::default()).unwrap();

    assert_eq!(report.asset_reports.len(), 2);

    // Find CKB report
    let ckb = report
        .asset_reports
        .iter()
        .find(|r| r.udt_type_script.is_none())
        .expect("CKB report");
    assert_eq!(ckb.daily_fee_sum, 30);
    assert_eq!(ckb.daily_event_count, 2);

    // Find UDT-A report
    let udt_report = report
        .asset_reports
        .iter()
        .find(|r| r.udt_type_script.is_some())
        .expect("UDT report");
    assert_eq!(udt_report.daily_fee_sum, 300);
    assert_eq!(udt_report.daily_event_count, 2);
}

#[tokio::test]
async fn test_fee_report_excludes_old_events() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Event older than 30 days — should not appear
    store.insert_forwarding_event(make_event(now - 31 * MILLIS_PER_DAY, 100));
    // Recent event
    store.insert_forwarding_event(make_event(now - 1000, 5));

    let report = fee_report_impl(&store, FeeReportParams::default()).unwrap();

    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert_eq!(ckb.monthly_fee_sum, 5);
    assert_eq!(ckb.monthly_event_count, 1);
}

#[tokio::test]
async fn test_forwarding_history_defaults() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    store.insert_forwarding_event(make_event(now - 5000, 1));
    store.insert_forwarding_event(make_event(now - 3000, 2));

    let result = forwarding_history_impl(&store, ForwardingHistoryParams::default()).unwrap();

    assert_eq!(result.total_count, 2);
    assert_eq!(result.events.len(), 2);
    assert_eq!(result.events[0].fee, 1);
    assert_eq!(result.events[1].fee, 2);
    // CKB events should have None udt_type_script
    assert!(result.events[0].udt_type_script.is_none());
}

#[tokio::test]
async fn test_forwarding_history_time_range() {
    let store = MockForwardingStore::new();

    store.insert_forwarding_event(make_event(100, 1));
    store.insert_forwarding_event(make_event(200, 2));
    store.insert_forwarding_event(make_event(300, 3));

    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            start_time: Some(200),
            end_time: Some(300),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.total_count, 2);
    assert_eq!(result.events[0].timestamp, 200);
    assert_eq!(result.events[1].timestamp, 300);
}

#[tokio::test]
async fn test_forwarding_history_pagination() {
    let store = MockForwardingStore::new();

    for i in 0..10u64 {
        store.insert_forwarding_event(make_event(100 + i, i as u128));
    }

    // First page
    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            end_time: Some(u64::MAX),
            limit: Some(3),
            after: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.events[0].timestamp, 100);
    assert_eq!(result.events[2].timestamp, 102);
    assert!(result.last_cursor.is_some());

    // Second page using the cursor from the first page
    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            end_time: Some(u64::MAX),
            limit: Some(3),
            after: result.last_cursor,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.events[0].timestamp, 103);
    assert_eq!(result.events[2].timestamp, 105);
}

#[tokio::test]
async fn test_forwarding_history_empty() {
    let store = MockForwardingStore::new();
    let result = forwarding_history_impl(&store, ForwardingHistoryParams::default()).unwrap();

    assert_eq!(result.total_count, 0);
    assert!(result.events.is_empty());
}

#[tokio::test]
async fn test_forwarding_history_filter_by_udt() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt_a = dummy_udt_script(1);
    let udt_b = dummy_udt_script(2);

    // CKB event
    store.insert_forwarding_event(make_event(now - 3000, 10));
    // UDT-A event
    store.insert_forwarding_event(make_event_with_udt(now - 2000, 20, Some(udt_a.clone())));
    // UDT-B event
    store.insert_forwarding_event(make_event_with_udt(now - 1000, 30, Some(udt_b.clone())));

    // Filter UDT-A only
    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            asset: Some(ForwardingHistoryAsset::Udt {
                udt_type_script: udt_a.into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 1);
    assert_eq!(result.events[0].fee, 20);
    assert!(result.events[0].udt_type_script.is_some());
}

#[tokio::test]
async fn test_forwarding_history_filter_ckb_only() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt = dummy_udt_script(9);

    store.insert_forwarding_event(make_event(now - 3000, 10));
    store.insert_forwarding_event(make_event_with_udt(now - 2000, 20, Some(udt)));

    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            asset: Some(ForwardingHistoryAsset::Ckb),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.total_count, 1);
    assert!(result.events[0].udt_type_script.is_none());
    assert_eq!(result.events[0].fee, 10);
}

#[tokio::test]
async fn test_forwarding_history_cursor_rejects_asset_mismatch() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt = dummy_udt_script(7);

    store.insert_forwarding_event(make_event_with_udt(now - 2000, 20, Some(udt.clone())));
    store.insert_forwarding_event(make_event_with_udt(now - 1000, 30, Some(udt.clone())));

    let first_page = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            asset: Some(ForwardingHistoryAsset::Udt {
                udt_type_script: udt.into(),
            }),
            limit: Some(1),
            ..Default::default()
        },
    )
    .unwrap();

    let err = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            asset: Some(ForwardingHistoryAsset::Ckb),
            after: first_page.last_cursor,
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("cursor does not match"));
}

#[tokio::test]
async fn test_forwarding_history_event_fields_mapped() {
    let store = MockForwardingStore::new();
    let now = now_timestamp_as_millis_u64();
    let udt = dummy_udt_script(42);
    let event = make_event_with_udt(now, 42, Some(udt.clone()));
    let expected_incoming_channel_id = event.incoming_channel_id;
    let expected_outgoing_channel_id = event.outgoing_channel_id;
    let expected_payment_hash = event.payment_hash;
    store.insert_forwarding_event(event);

    let result = forwarding_history_impl(&store, ForwardingHistoryParams::default()).unwrap();

    assert_eq!(result.events.len(), 1);
    let info = &result.events[0];
    assert_eq!(info.timestamp, now);
    assert_eq!(info.incoming_amount, 1042);
    assert_eq!(info.outgoing_amount, 1000);
    assert_eq!(info.fee, 42);
    assert_eq!(
        info.incoming_channel_id,
        expected_incoming_channel_id.into()
    );
    assert_eq!(
        info.outgoing_channel_id,
        expected_outgoing_channel_id.into()
    );
    assert_eq!(info.payment_hash, expected_payment_hash.into());
    assert!(info.udt_type_script.is_some());
}

// ─── PaymentEvent RPC tests ────────────────────────────────────────────────

fn make_payment_event(timestamp: u64, amount: u128, event_type: PaymentEventType) -> PaymentEvent {
    make_payment_event_with_udt(timestamp, amount, event_type, None)
}

fn make_payment_event_with_udt(
    timestamp: u64,
    amount: u128,
    event_type: PaymentEventType,
    udt_type_script: Option<Script>,
) -> PaymentEvent {
    PaymentEvent {
        event_type,
        timestamp,
        channel_id: gen_rand_sha256_hash(),
        amount,
        fee: if matches!(event_type, PaymentEventType::Send) {
            10
        } else {
            0
        },
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script,
    }
}

// ─── sent_payment_report_impl tests ─────────────────────────────────────────

#[tokio::test]
async fn test_sent_payment_report_empty() {
    let store = MockForwardingStore::new();
    let report = sent_payment_report_impl(&store).unwrap();
    assert!(report.asset_reports.is_empty());
}

#[tokio::test]
async fn test_sent_payment_report_ckb_only() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Send events at different time windows
    store.insert_payment_event(make_payment_event(
        now - MILLIS_PER_DAY / 2,
        100,
        PaymentEventType::Send,
    ));
    store.insert_payment_event(make_payment_event(
        now - 3 * MILLIS_PER_DAY,
        200,
        PaymentEventType::Send,
    ));
    store.insert_payment_event(make_payment_event(
        now - 15 * MILLIS_PER_DAY,
        300,
        PaymentEventType::Send,
    ));

    // Add a receive event — should NOT appear in sent report
    store.insert_payment_event(make_payment_event(
        now - 1000,
        999,
        PaymentEventType::Receive,
    ));

    let report = sent_payment_report_impl(&store).unwrap();
    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert!(ckb.udt_type_script.is_none());
    assert_eq!(ckb.daily_amount_sum, 100);
    assert_eq!(ckb.daily_event_count, 1);
    assert_eq!(ckb.weekly_amount_sum, 300); // day + week
    assert_eq!(ckb.weekly_event_count, 2);
    assert_eq!(ckb.monthly_amount_sum, 600); // day + week + month
    assert_eq!(ckb.monthly_event_count, 3);
}

#[tokio::test]
async fn test_sent_payment_report_multiple_assets() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt_a = dummy_udt_script(1);

    // CKB send events
    store.insert_payment_event(make_payment_event(now - 1000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(now - 2000, 200, PaymentEventType::Send));

    // UDT-A send events
    store.insert_payment_event(make_payment_event_with_udt(
        now - 1000,
        1000,
        PaymentEventType::Send,
        Some(udt_a.clone()),
    ));

    let report = sent_payment_report_impl(&store).unwrap();
    assert_eq!(report.asset_reports.len(), 2);

    let ckb = report
        .asset_reports
        .iter()
        .find(|r| r.udt_type_script.is_none())
        .expect("CKB report");
    assert_eq!(ckb.daily_amount_sum, 300);
    assert_eq!(ckb.daily_event_count, 2);

    let udt_report = report
        .asset_reports
        .iter()
        .find(|r| r.udt_type_script.is_some())
        .expect("UDT report");
    assert_eq!(udt_report.daily_amount_sum, 1000);
    assert_eq!(udt_report.daily_event_count, 1);
}

#[tokio::test]
async fn test_sent_payment_report_excludes_old_events() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Event older than 30 days
    store.insert_payment_event(make_payment_event(
        now - 31 * MILLIS_PER_DAY,
        1000,
        PaymentEventType::Send,
    ));
    // Recent event
    store.insert_payment_event(make_payment_event(now - 1000, 50, PaymentEventType::Send));

    let report = sent_payment_report_impl(&store).unwrap();
    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert_eq!(ckb.monthly_amount_sum, 50);
    assert_eq!(ckb.monthly_event_count, 1);
}

// ─── received_payment_report_impl tests ─────────────────────────────────────

#[tokio::test]
async fn test_received_payment_report_empty() {
    let store = MockForwardingStore::new();
    let report = received_payment_report_impl(&store).unwrap();
    assert!(report.asset_reports.is_empty());
}

#[tokio::test]
async fn test_received_payment_report_ckb_only() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Receive events at different time windows
    store.insert_payment_event(make_payment_event(
        now - MILLIS_PER_DAY / 2,
        100,
        PaymentEventType::Receive,
    ));
    store.insert_payment_event(make_payment_event(
        now - 3 * MILLIS_PER_DAY,
        200,
        PaymentEventType::Receive,
    ));
    store.insert_payment_event(make_payment_event(
        now - 15 * MILLIS_PER_DAY,
        300,
        PaymentEventType::Receive,
    ));

    // Add a send event — should NOT appear in received report
    store.insert_payment_event(make_payment_event(now - 1000, 999, PaymentEventType::Send));

    let report = received_payment_report_impl(&store).unwrap();
    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert!(ckb.udt_type_script.is_none());
    assert_eq!(ckb.daily_amount_sum, 100);
    assert_eq!(ckb.daily_event_count, 1);
    assert_eq!(ckb.weekly_amount_sum, 300);
    assert_eq!(ckb.weekly_event_count, 2);
    assert_eq!(ckb.monthly_amount_sum, 600);
    assert_eq!(ckb.monthly_event_count, 3);
}

#[tokio::test]
async fn test_received_payment_report_filters_only_receive_type() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    // Mixed events
    store.insert_payment_event(make_payment_event(now - 1000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(
        now - 2000,
        200,
        PaymentEventType::Receive,
    ));
    store.insert_payment_event(make_payment_event(
        now - 3000,
        300,
        PaymentEventType::Receive,
    ));

    let report = received_payment_report_impl(&store).unwrap();
    assert_eq!(report.asset_reports.len(), 1);
    let ckb = &report.asset_reports[0];
    assert_eq!(ckb.daily_amount_sum, 500); // only receive: 200 + 300
    assert_eq!(ckb.daily_event_count, 2);
}

// ─── payment_history_impl tests ─────────────────────────────────────────────

#[tokio::test]
async fn test_payment_history_empty() {
    let store = MockForwardingStore::new();
    let result = payment_history_impl(&store, PaymentHistoryParams::default()).unwrap();
    assert_eq!(result.total_count, 0);
    assert!(result.events.is_empty());
}

#[tokio::test]
async fn test_payment_history_defaults() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    store.insert_payment_event(make_payment_event(now - 5000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(
        now - 3000,
        200,
        PaymentEventType::Receive,
    ));

    let result = payment_history_impl(&store, PaymentHistoryParams::default()).unwrap();
    assert_eq!(result.total_count, 2);
    assert_eq!(result.events.len(), 2);
    assert_eq!(result.events[0].amount, 100);
    assert_eq!(result.events[0].event_type, "Send");
    assert_eq!(result.events[1].amount, 200);
    assert_eq!(result.events[1].event_type, "Receive");
}

#[tokio::test]
async fn test_payment_history_time_range() {
    let store = MockForwardingStore::new();

    store.insert_payment_event(make_payment_event(100, 10, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(200, 20, PaymentEventType::Receive));
    store.insert_payment_event(make_payment_event(300, 30, PaymentEventType::Send));

    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            start_time: Some(200),
            end_time: Some(300),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.total_count, 2);
    assert_eq!(result.events[0].timestamp, 200);
    assert_eq!(result.events[1].timestamp, 300);
}

#[tokio::test]
async fn test_payment_history_pagination() {
    let store = MockForwardingStore::new();

    for i in 0..10u64 {
        let event_type = if i % 2 == 0 {
            PaymentEventType::Send
        } else {
            PaymentEventType::Receive
        };
        store.insert_payment_event(make_payment_event(100 + i, i as u128 * 100, event_type));
    }

    // First page
    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            end_time: Some(u64::MAX),
            limit: Some(3),
            after: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.events[0].timestamp, 100);
    assert_eq!(result.events[2].timestamp, 102);
    assert!(result.last_cursor.is_some());

    // Second page using cursor from first page
    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            end_time: Some(u64::MAX),
            limit: Some(3),
            after: result.last_cursor,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.events[0].timestamp, 103);
    assert_eq!(result.events[2].timestamp, 105);
}

#[tokio::test]
async fn test_payment_history_filter_by_udt() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt_a = dummy_udt_script(1);
    let udt_b = dummy_udt_script(2);

    // CKB event
    store.insert_payment_event(make_payment_event(now - 3000, 100, PaymentEventType::Send));
    // UDT-A event
    store.insert_payment_event(make_payment_event_with_udt(
        now - 2000,
        200,
        PaymentEventType::Receive,
        Some(udt_a.clone()),
    ));
    // UDT-B event
    store.insert_payment_event(make_payment_event_with_udt(
        now - 1000,
        300,
        PaymentEventType::Send,
        Some(udt_b.clone()),
    ));

    // Filter UDT-A only
    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            asset: Some(PaymentHistoryAsset::Udt {
                udt_type_script: udt_a.into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 1);
    assert_eq!(result.events[0].amount, 200);
    assert_eq!(result.events[0].event_type, "Receive");
    assert!(result.events[0].udt_type_script.is_some());
}

#[tokio::test]
async fn test_payment_history_filter_ckb_only() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();
    let udt = dummy_udt_script(3);

    store.insert_payment_event(make_payment_event(now - 3000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event_with_udt(
        now - 2000,
        200,
        PaymentEventType::Receive,
        Some(udt),
    ));

    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            asset: Some(PaymentHistoryAsset::Ckb),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.total_count, 1);
    assert_eq!(result.events[0].amount, 100);
    assert!(result.events[0].udt_type_script.is_none());
}

#[tokio::test]
async fn test_payment_history_filter_by_event_type() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    store.insert_payment_event(make_payment_event(now - 3000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(
        now - 2000,
        200,
        PaymentEventType::Receive,
    ));

    let result = payment_history_impl(
        &store,
        PaymentHistoryParams {
            event_type: Some(PaymentHistoryEventType::Receive),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.total_count, 1);
    assert_eq!(result.events[0].event_type, "Receive");
    assert_eq!(result.events[0].amount, 200);
}

#[tokio::test]
async fn test_payment_history_cursor_rejects_filter_mismatch() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    store.insert_payment_event(make_payment_event(now - 3000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(
        now - 2000,
        200,
        PaymentEventType::Receive,
    ));

    let first_page = payment_history_impl(
        &store,
        PaymentHistoryParams {
            event_type: Some(PaymentHistoryEventType::Receive),
            limit: Some(1),
            ..Default::default()
        },
    )
    .unwrap();

    let err = payment_history_impl(
        &store,
        PaymentHistoryParams {
            event_type: Some(PaymentHistoryEventType::Send),
            after: first_page.last_cursor,
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("cursor does not match"));
}

#[tokio::test]
async fn test_payment_history_event_fields_mapped() {
    let store = MockForwardingStore::new();
    let now = now_timestamp_as_millis_u64();
    let udt = dummy_udt_script(42);
    let event = make_payment_event_with_udt(now, 500, PaymentEventType::Send, Some(udt.clone()));
    let expected_channel_id = event.channel_id;
    let expected_payment_hash = event.payment_hash;
    store.insert_payment_event(event);

    let result = payment_history_impl(&store, PaymentHistoryParams::default()).unwrap();

    assert_eq!(result.events.len(), 1);
    let info = &result.events[0];
    assert_eq!(info.event_type, "Send");
    assert_eq!(info.timestamp, now);
    assert_eq!(info.amount, 500);
    assert_eq!(info.fee, 10);
    assert_eq!(info.channel_id, expected_channel_id.into());
    assert_eq!(info.payment_hash, expected_payment_hash.into());
    assert!(info.udt_type_script.is_some());
}

#[tokio::test]
async fn test_payment_history_returns_both_types() {
    let now = now_timestamp_as_millis_u64();
    let store = MockForwardingStore::new();

    store.insert_payment_event(make_payment_event(now - 2000, 100, PaymentEventType::Send));
    store.insert_payment_event(make_payment_event(
        now - 1000,
        200,
        PaymentEventType::Receive,
    ));

    // payment_history returns both Send and Receive events
    let result = payment_history_impl(&store, PaymentHistoryParams::default()).unwrap();
    assert_eq!(result.total_count, 2);

    let types: Vec<&str> = result
        .events
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(types.contains(&"Send"));
    assert!(types.contains(&"Receive"));
}
