use crate::fiber::channel::ForwardingEventStore;
use crate::gen_rand_sha256_hash;
use crate::now_timestamp_as_millis_u64;
use crate::rpc::info::{
    fee_report_impl, forwarding_history_impl, ForwardingHistoryParams, MILLIS_PER_DAY,
};
use ckb_types::packed::Script;
use fiber_types::ForwardingEvent;
use std::sync::Mutex;

/// A simple in-memory mock store for testing the fee RPC logic
/// without requiring RocksDB.
struct MockForwardingStore {
    events: Mutex<Vec<ForwardingEvent>>,
}

impl MockForwardingStore {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}

impl ForwardingEventStore for MockForwardingStore {
    fn insert_forwarding_event(&self, event: ForwardingEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn get_forwarding_events(
        &self,
        start_time: u64,
        end_time: u64,
        limit: usize,
        offset: usize,
    ) -> Vec<ForwardingEvent> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .filter(|e| e.timestamp >= start_time && e.timestamp <= end_time)
            .skip(offset)
            .take(limit)
            .cloned()
            .collect()
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
    let report = fee_report_impl(&store).unwrap();

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

    let report = fee_report_impl(&store).unwrap();

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

    let report = fee_report_impl(&store).unwrap();

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

    let report = fee_report_impl(&store).unwrap();

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
            offset: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.events[0].timestamp, 100);
    assert_eq!(result.events[2].timestamp, 102);

    // Second page
    let result = forwarding_history_impl(
        &store,
        ForwardingHistoryParams {
            end_time: Some(u64::MAX),
            limit: Some(3),
            offset: Some(3),
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
            udt_type_script: Some(udt_a.into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.total_count, 1);
    assert_eq!(result.events[0].fee, 20);
    assert!(result.events[0].udt_type_script.is_some());
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
