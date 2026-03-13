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
pub(crate) const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
pub(crate) const MILLIS_PER_WEEK: u64 = 7 * MILLIS_PER_DAY;
pub(crate) const MILLIS_PER_MONTH: u64 = 30 * MILLIS_PER_DAY;

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
