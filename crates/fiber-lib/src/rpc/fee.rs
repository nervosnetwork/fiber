use crate::fiber::channel::ForwardingEventStore;
use crate::now_timestamp_as_millis_u64;
use ckb_types::packed::Script;
use ckb_types::prelude::Entity;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use std::collections::BTreeMap;

pub use fiber_json_types::{
    AssetFeeReport, FeeReportResult, ForwardingEventInfo, ForwardingHistoryParams,
    ForwardingHistoryResult,
};

const DEFAULT_FORWARDING_HISTORY_LIMIT: u64 = 100;
const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
const MILLIS_PER_WEEK: u64 = 7 * MILLIS_PER_DAY;
const MILLIS_PER_MONTH: u64 = 30 * MILLIS_PER_DAY;

/// RPC module for querying forwarding fees and history.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait FeeRpc {
    /// Returns a summary of forwarding fees earned over day/week/month windows,
    /// grouped by asset type (CKB and each UDT).
    #[method(name = "fee_report")]
    async fn fee_report(&self) -> Result<FeeReportResult, ErrorObjectOwned>;

    /// Returns individual forwarding events with optional time range, asset filter,
    /// and pagination.
    #[method(name = "forwarding_history")]
    async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned>;
}

pub struct FeeRpcServerImpl<S> {
    store: S,
}

impl<S> FeeRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> FeeRpcServer for FeeRpcServerImpl<S>
where
    S: ForwardingEventStore + Send + Sync + 'static,
{
    async fn fee_report(&self) -> Result<FeeReportResult, ErrorObjectOwned> {
        self.fee_report().await
    }

    async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned> {
        self.forwarding_history(params).await
    }
}

/// Accumulator for per-asset fee aggregation across time windows.
#[derive(Default)]
struct AssetAccum {
    daily_fee_sum: u128,
    weekly_fee_sum: u128,
    monthly_fee_sum: u128,
    daily_event_count: u64,
    weekly_event_count: u64,
    monthly_event_count: u64,
}

/// Key for grouping: serialized `Option<Script>` bytes.
/// We use `Vec<u8>` so we can use BTreeMap for deterministic ordering.
fn asset_key(udt_type_script: &Option<Script>) -> Vec<u8> {
    match udt_type_script {
        None => vec![0],
        Some(script) => {
            let mut key = vec![1];
            key.extend_from_slice(script.as_slice());
            key
        }
    }
}

impl<S> FeeRpcServerImpl<S>
where
    S: ForwardingEventStore + Send + Sync + 'static,
{
    pub async fn fee_report(&self) -> Result<FeeReportResult, ErrorObjectOwned> {
        let now = now_timestamp_as_millis_u64();
        let day_ago = now.saturating_sub(MILLIS_PER_DAY);
        let week_ago = now.saturating_sub(MILLIS_PER_WEEK);
        let month_ago = now.saturating_sub(MILLIS_PER_MONTH);

        // Fetch the last 30 days of events (superset of daily/weekly)
        let events = self
            .store
            .get_forwarding_events(month_ago, now, usize::MAX, 0);

        // Group by asset type
        let mut accums: BTreeMap<Vec<u8>, (Option<Script>, AssetAccum)> = BTreeMap::new();

        for event in &events {
            let key = asset_key(&event.udt_type_script);
            let entry = accums
                .entry(key)
                .or_insert_with(|| (event.udt_type_script.clone(), AssetAccum::default()));
            let accum = &mut entry.1;

            accum.monthly_fee_sum = accum.monthly_fee_sum.saturating_add(event.fee);
            accum.monthly_event_count = accum.monthly_event_count.saturating_add(1);
            if event.timestamp >= week_ago {
                accum.weekly_fee_sum = accum.weekly_fee_sum.saturating_add(event.fee);
                accum.weekly_event_count = accum.weekly_event_count.saturating_add(1);
            }
            if event.timestamp >= day_ago {
                accum.daily_fee_sum = accum.daily_fee_sum.saturating_add(event.fee);
                accum.daily_event_count = accum.daily_event_count.saturating_add(1);
            }
        }

        let asset_reports = accums
            .into_values()
            .map(|(udt_type_script, accum)| AssetFeeReport {
                udt_type_script: udt_type_script.map(Into::into),
                daily_fee_sum: accum.daily_fee_sum,
                weekly_fee_sum: accum.weekly_fee_sum,
                monthly_fee_sum: accum.monthly_fee_sum,
                daily_event_count: accum.daily_event_count,
                weekly_event_count: accum.weekly_event_count,
                monthly_event_count: accum.monthly_event_count,
            })
            .collect();

        Ok(FeeReportResult { asset_reports })
    }

    pub async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned> {
        let now = now_timestamp_as_millis_u64();
        let start_time = params.start_time.unwrap_or(0);
        let end_time = params.end_time.unwrap_or(now);
        let limit = params.limit.unwrap_or(DEFAULT_FORWARDING_HISTORY_LIMIT) as usize;
        let offset = params.offset.unwrap_or(0) as usize;

        // Convert the optional RPC Script filter to packed::Script for comparison
        let udt_filter: Option<Option<Script>> = params.udt_type_script.map(|s| Some(s.into()));

        let events = self
            .store
            .get_forwarding_events(start_time, end_time, usize::MAX, 0);

        // Apply asset filter, then pagination
        let filtered: Vec<_> = events
            .into_iter()
            .filter(|e| match &udt_filter {
                None => true, // no filter, return all
                Some(script) => &e.udt_type_script == script,
            })
            .skip(offset)
            .take(limit)
            .collect();

        let total_count = filtered.len() as u64;
        let events = filtered
            .into_iter()
            .map(|e| ForwardingEventInfo {
                timestamp: e.timestamp,
                incoming_channel_id: e.incoming_channel_id.into(),
                outgoing_channel_id: e.outgoing_channel_id.into(),
                incoming_amount: e.incoming_amount,
                outgoing_amount: e.outgoing_amount,
                fee: e.fee,
                payment_hash: e.payment_hash.into(),
                udt_type_script: e.udt_type_script.map(Into::into),
            })
            .collect();

        Ok(ForwardingHistoryResult {
            events,
            total_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        use crate::gen_rand_sha256_hash;
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
        let rpc = FeeRpcServerImpl::new(store);
        let report = rpc.fee_report().await.unwrap();

        assert!(report.asset_reports.is_empty());
    }

    #[tokio::test]
    async fn test_fee_report_ckb_only() {
        let now = crate::now_timestamp_as_millis_u64();
        let store = MockForwardingStore::new();

        // Event within last day (CKB)
        store.insert_forwarding_event(make_event(now - MILLIS_PER_DAY / 2, 10));
        // Event within last week but not last day (CKB)
        store.insert_forwarding_event(make_event(now - 3 * MILLIS_PER_DAY, 20));
        // Event within last month but not last week (CKB)
        store.insert_forwarding_event(make_event(now - 15 * MILLIS_PER_DAY, 30));

        let rpc = FeeRpcServerImpl::new(store);
        let report = rpc.fee_report().await.unwrap();

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
        let now = crate::now_timestamp_as_millis_u64();
        let store = MockForwardingStore::new();
        let udt_a = dummy_udt_script(1);

        // CKB events
        store.insert_forwarding_event(make_event(now - 1000, 10));
        store.insert_forwarding_event(make_event(now - 2000, 20));

        // UDT-A events
        store.insert_forwarding_event(make_event_with_udt(now - 1000, 100, Some(udt_a.clone())));
        store.insert_forwarding_event(make_event_with_udt(now - 2000, 200, Some(udt_a.clone())));

        let rpc = FeeRpcServerImpl::new(store);
        let report = rpc.fee_report().await.unwrap();

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
        let now = crate::now_timestamp_as_millis_u64();
        let store = MockForwardingStore::new();

        // Event older than 30 days — should not appear
        store.insert_forwarding_event(make_event(now - 31 * MILLIS_PER_DAY, 100));
        // Recent event
        store.insert_forwarding_event(make_event(now - 1000, 5));

        let rpc = FeeRpcServerImpl::new(store);
        let report = rpc.fee_report().await.unwrap();

        assert_eq!(report.asset_reports.len(), 1);
        let ckb = &report.asset_reports[0];
        assert_eq!(ckb.monthly_fee_sum, 5);
        assert_eq!(ckb.monthly_event_count, 1);
    }

    #[tokio::test]
    async fn test_forwarding_history_defaults() {
        let now = crate::now_timestamp_as_millis_u64();
        let store = MockForwardingStore::new();

        store.insert_forwarding_event(make_event(now - 5000, 1));
        store.insert_forwarding_event(make_event(now - 3000, 2));

        let rpc = FeeRpcServerImpl::new(store);
        let result = rpc
            .forwarding_history(ForwardingHistoryParams::default())
            .await
            .unwrap();

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

        let rpc = FeeRpcServerImpl::new(store);
        let result = rpc
            .forwarding_history(ForwardingHistoryParams {
                start_time: Some(200),
                end_time: Some(300),
                ..Default::default()
            })
            .await
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

        let rpc = FeeRpcServerImpl::new(store);

        // First page
        let result = rpc
            .forwarding_history(ForwardingHistoryParams {
                end_time: Some(u64::MAX),
                limit: Some(3),
                offset: Some(0),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.total_count, 3);
        assert_eq!(result.events[0].timestamp, 100);
        assert_eq!(result.events[2].timestamp, 102);

        // Second page
        let result = rpc
            .forwarding_history(ForwardingHistoryParams {
                end_time: Some(u64::MAX),
                limit: Some(3),
                offset: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.total_count, 3);
        assert_eq!(result.events[0].timestamp, 103);
        assert_eq!(result.events[2].timestamp, 105);
    }

    #[tokio::test]
    async fn test_forwarding_history_empty() {
        let store = MockForwardingStore::new();
        let rpc = FeeRpcServerImpl::new(store);
        let result = rpc
            .forwarding_history(ForwardingHistoryParams::default())
            .await
            .unwrap();

        assert_eq!(result.total_count, 0);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn test_forwarding_history_filter_by_udt() {
        let now = crate::now_timestamp_as_millis_u64();
        let store = MockForwardingStore::new();
        let udt_a = dummy_udt_script(1);
        let udt_b = dummy_udt_script(2);

        // CKB event
        store.insert_forwarding_event(make_event(now - 3000, 10));
        // UDT-A event
        store.insert_forwarding_event(make_event_with_udt(now - 2000, 20, Some(udt_a.clone())));
        // UDT-B event
        store.insert_forwarding_event(make_event_with_udt(now - 1000, 30, Some(udt_b.clone())));

        let rpc = FeeRpcServerImpl::new(store);

        // Filter UDT-A only
        let result = rpc
            .forwarding_history(ForwardingHistoryParams {
                udt_type_script: Some(udt_a.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.total_count, 1);
        assert_eq!(result.events[0].fee, 20);
        assert!(result.events[0].udt_type_script.is_some());
    }

    #[tokio::test]
    async fn test_forwarding_history_event_fields_mapped() {
        let store = MockForwardingStore::new();
        let now = crate::now_timestamp_as_millis_u64();
        let udt = dummy_udt_script(42);
        let event = make_event_with_udt(now, 42, Some(udt.clone()));
        let expected_incoming_channel_id = event.incoming_channel_id;
        let expected_outgoing_channel_id = event.outgoing_channel_id;
        let expected_payment_hash = event.payment_hash;
        store.insert_forwarding_event(event);

        let rpc = FeeRpcServerImpl::new(store);
        let result = rpc
            .forwarding_history(ForwardingHistoryParams::default())
            .await
            .unwrap();

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
}
