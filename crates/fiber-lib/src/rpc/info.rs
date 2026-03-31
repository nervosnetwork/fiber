use crate::ckb::CkbConfig;
use crate::fiber::channel::{
    AssetSelector, ForwardingHistoryCursor, ForwardingHistoryQuery, PaymentEventStore,
    PaymentHistoryCursor, PaymentHistoryQuery,
};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
use crate::{handle_actor_call, now_timestamp_as_millis_u64};
use ckb_jsonrpc_types::{JsonBytes, Script};
use ckb_types::prelude::Entity;
use fiber_types::PaymentEventType;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use std::collections::BTreeMap;

use ractor::{call, ActorRef};

pub use fiber_json_types::{
    AssetFeeReport, AssetPaymentReport, FeeReportParams, FeeReportResult, ForwardingEventInfo,
    ForwardingHistoryAsset, ForwardingHistoryParams, ForwardingHistoryResult, NodeInfoResult,
    PaymentEventInfo, PaymentHistoryAsset, PaymentHistoryEventType, PaymentHistoryParams,
    PaymentHistoryResult, ReceivedPaymentReportResult, SentPaymentReportResult,
};

const DEFAULT_FORWARDING_HISTORY_LIMIT: u64 = 100;
const DEFAULT_PAYMENT_HISTORY_LIMIT: u64 = 100;
const MAX_FORWARDING_HISTORY_LIMIT: u64 = 10000;
const MAX_PAYMENT_HISTORY_LIMIT: u64 = 10000;
/// Batch size for streaming full-table scans in report RPCs.
/// Large enough to amortize RocksDB overhead; small enough to bound memory per batch.
const REPORT_BATCH_SIZE: usize = 1000;
pub(crate) const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
pub(crate) const MILLIS_PER_WEEK: u64 = 7 * MILLIS_PER_DAY;
pub(crate) const MILLIS_PER_MONTH: u64 = 30 * MILLIS_PER_DAY;

pub struct InfoRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    default_funding_lock_script: Script,
    store: S,
}

impl<S> InfoRpcServerImpl<S> {
    #[allow(unused_variables)]
    pub fn new(actor: ActorRef<NetworkActorMessage>, config: CkbConfig, store: S) -> Self {
        #[cfg(not(test))]
        let default_funding_lock_script = config
            .get_default_funding_lock_script()
            .expect("get default funding lock script should be ok")
            .into();

        // `decrypt_from_file` is invoked in `get_default_funding_lock_script`,
        // which will cost more than 30 seconds, so we mock it in tests.
        #[cfg(test)]
        let default_funding_lock_script = Default::default();

        InfoRpcServerImpl {
            actor,
            default_funding_lock_script,
            store,
        }
    }
}

/// The RPC module for node information.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait InfoRpc {
    /// Get the node information.
    #[method(name = "node_info")]
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned>;

    /// Returns a summary of forwarding fees earned over day/week/month windows,
    /// grouped by asset type (CKB and each UDT).
    #[method(name = "fee_report")]
    async fn fee_report(
        &self,
        params: FeeReportParams,
    ) -> Result<FeeReportResult, ErrorObjectOwned>;

    /// Returns individual forwarding events with optional time range, asset filter,
    /// and pagination.
    #[method(name = "forwarding_history")]
    async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned>;

    /// Returns a summary of sent payment amounts over day/week/month windows,
    /// grouped by asset type (CKB and each UDT).
    #[method(name = "sent_payment_report")]
    async fn sent_payment_report(&self) -> Result<SentPaymentReportResult, ErrorObjectOwned>;

    /// Returns a summary of received payment amounts over day/week/month windows,
    /// grouped by asset type (CKB and each UDT).
    #[method(name = "received_payment_report")]
    async fn received_payment_report(
        &self,
    ) -> Result<ReceivedPaymentReportResult, ErrorObjectOwned>;

    /// Returns individual send/receive payment events with optional time range,
    /// asset filter, and pagination.
    #[method(name = "payment_history")]
    async fn payment_history(
        &self,
        params: PaymentHistoryParams,
    ) -> Result<PaymentHistoryResult, ErrorObjectOwned>;
}

#[async_trait::async_trait]
#[cfg(not(target_arch = "wasm32"))]
impl<S> InfoRpcServer for InfoRpcServerImpl<S>
where
    S: PaymentEventStore + Send + Sync + 'static,
{
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        self.node_info().await
    }

    async fn fee_report(
        &self,
        params: FeeReportParams,
    ) -> Result<FeeReportResult, ErrorObjectOwned> {
        self.fee_report(params).await
    }

    async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned> {
        self.forwarding_history(params).await
    }

    async fn sent_payment_report(&self) -> Result<SentPaymentReportResult, ErrorObjectOwned> {
        self.sent_payment_report().await
    }

    async fn received_payment_report(
        &self,
    ) -> Result<ReceivedPaymentReportResult, ErrorObjectOwned> {
        self.received_payment_report().await
    }

    async fn payment_history(
        &self,
        params: PaymentHistoryParams,
    ) -> Result<PaymentHistoryResult, ErrorObjectOwned> {
        self.payment_history(params).await
    }
}

impl<S> InfoRpcServerImpl<S>
where
    S: PaymentEventStore + Send + Sync + 'static,
{
    pub async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let commit_hash = crate::get_git_commit_info();

        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));

        handle_actor_call!(self.actor, message, ()).map(|response| NodeInfoResult {
            version,
            commit_hash,
            features: response.features.enabled_features_names(),
            pubkey: response.node_id.into(),
            node_name: response.node_name.map(|name| name.to_string()),
            addresses: response.addresses.iter().map(|a| a.to_string()).collect(),
            chain_hash: response.chain_hash.into(),
            open_channel_auto_accept_min_ckb_funding_amount: response
                .open_channel_auto_accept_min_ckb_funding_amount,
            auto_accept_channel_ckb_funding_amount: response.auto_accept_channel_ckb_funding_amount,
            default_funding_lock_script: self.default_funding_lock_script.clone(),
            tlc_expiry_delta: response.tlc_expiry_delta,
            tlc_min_value: response.tlc_min_value,
            tlc_fee_proportional_millionths: response.tlc_fee_proportional_millionths,
            channel_count: response.channel_count,
            pending_channel_count: response.pending_channel_count,
            peers_count: response.peers_count,
            udt_cfg_infos: response.udt_cfg_infos.into(),
        })
    }

    pub async fn fee_report(
        &self,
        params: FeeReportParams,
    ) -> Result<FeeReportResult, ErrorObjectOwned> {
        fee_report_impl(&self.store, params)
    }

    pub async fn forwarding_history(
        &self,
        params: ForwardingHistoryParams,
    ) -> Result<ForwardingHistoryResult, ErrorObjectOwned> {
        forwarding_history_impl(&self.store, params)
    }

    pub async fn sent_payment_report(&self) -> Result<SentPaymentReportResult, ErrorObjectOwned> {
        sent_payment_report_impl(&self.store)
    }

    pub async fn received_payment_report(
        &self,
    ) -> Result<ReceivedPaymentReportResult, ErrorObjectOwned> {
        received_payment_report_impl(&self.store)
    }

    pub async fn payment_history(
        &self,
        params: PaymentHistoryParams,
    ) -> Result<PaymentHistoryResult, ErrorObjectOwned> {
        payment_history_impl(&self.store, params)
    }
}

/// Core fee report logic, usable from both the RPC impl and tests.
pub fn fee_report_impl(
    store: &impl PaymentEventStore,
    params: FeeReportParams,
) -> Result<FeeReportResult, ErrorObjectOwned> {
    let now = now_timestamp_as_millis_u64();

    let days = params.days.unwrap_or(30);
    if days > 90 {
        return Err(ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            "days parameter exceeds maximum allowed value (90). Use a shorter time range.",
            Option::<()>::None,
        ));
    }

    let start_time = params
        .start_time
        .unwrap_or(now.saturating_sub(days * MILLIS_PER_DAY));
    let end_time = params.end_time.unwrap_or(now);

    let day_ago = now.saturating_sub(MILLIS_PER_DAY);
    let week_ago = now.saturating_sub(MILLIS_PER_WEEK);

    // Stream events in batches to avoid loading the entire time window into memory at once.
    let mut accums: BTreeMap<Vec<u8>, (Option<ckb_types::packed::Script>, AssetAccum)> =
        BTreeMap::new();
    let mut cursor: Option<ForwardingHistoryCursor> = None;
    loop {
        let (batch, next_cursor) = store
            .query_forwarding_events(ForwardingHistoryQuery {
                asset: AssetSelector::All,
                start_time,
                end_time,
                limit: REPORT_BATCH_SIZE,
                after: cursor,
            })
            .map_err(crate::rpc::utils::rpc_error_no_data)?;
        let done = next_cursor.is_none() || batch.is_empty();
        for event in &batch {
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
        if done {
            break;
        }
        cursor = next_cursor;
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

/// Core forwarding history logic, usable from both the RPC impl and tests.
pub fn forwarding_history_impl(
    store: &impl PaymentEventStore,
    params: ForwardingHistoryParams,
) -> Result<ForwardingHistoryResult, ErrorObjectOwned> {
    let now = now_timestamp_as_millis_u64();
    let start_time = params.start_time.unwrap_or(0);
    let end_time = params.end_time.unwrap_or(now);
    let limit = params.limit.unwrap_or(DEFAULT_FORWARDING_HISTORY_LIMIT) as usize;

    if limit > MAX_FORWARDING_HISTORY_LIMIT as usize {
        return Err(ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!(
                "limit exceeds maximum allowed value ({}). \
                Use a smaller limit or use start_time/end_time to narrow the time range.",
                MAX_FORWARDING_HISTORY_LIMIT
            ),
            Option::<()>::None,
        ));
    }

    let asset = forwarding_history_asset_selector(&params)?;
    let after_cursor = params
        .after
        .as_ref()
        .map(|bytes| ForwardingHistoryCursor::from_bytes(bytes.as_bytes()))
        .transpose()
        .map_err(|err| ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err, Some(&params)))?;

    let query = ForwardingHistoryQuery {
        asset,
        start_time,
        end_time,
        limit,
        after: after_cursor,
    };
    let (events, store_cursor) = store
        .query_forwarding_events(query)
        .map_err(|err| ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err, Some(&params)))?;

    let total_count = events.len() as u64;
    let last_cursor = store_cursor.map(|cursor| JsonBytes::from_vec(cursor.to_bytes()));
    let events = events
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
        last_cursor,
    })
}

fn forwarding_history_asset_selector(
    params: &ForwardingHistoryParams,
) -> Result<AssetSelector, ErrorObjectOwned> {
    if params.asset.is_some() && params.udt_type_script.is_some() {
        return Err(ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            "use either `asset` or deprecated `udt_type_script`, not both",
            Some(params),
        ));
    }

    let selector = match &params.asset {
        Some(ForwardingHistoryAsset::Ckb) => AssetSelector::Ckb,
        Some(ForwardingHistoryAsset::Udt { udt_type_script }) => {
            AssetSelector::Udt(udt_type_script.clone().into())
        }
        None => match &params.udt_type_script {
            Some(script) => AssetSelector::Udt(script.clone().into()),
            None => AssetSelector::All,
        },
    };

    Ok(selector)
}

/// Core sent payment report logic.
pub fn sent_payment_report_impl(
    store: &impl PaymentEventStore,
) -> Result<SentPaymentReportResult, ErrorObjectOwned> {
    payment_report_impl(store, PaymentEventType::Send)
        .map(|asset_reports| SentPaymentReportResult { asset_reports })
}

/// Core received payment report logic.
pub fn received_payment_report_impl(
    store: &impl PaymentEventStore,
) -> Result<ReceivedPaymentReportResult, ErrorObjectOwned> {
    payment_report_impl(store, PaymentEventType::Receive)
        .map(|asset_reports| ReceivedPaymentReportResult { asset_reports })
}

/// Shared logic for sent/received payment reports.
fn payment_report_impl(
    store: &impl PaymentEventStore,
    event_type: PaymentEventType,
) -> Result<Vec<AssetPaymentReport>, ErrorObjectOwned> {
    let now = now_timestamp_as_millis_u64();
    let day_ago = now.saturating_sub(MILLIS_PER_DAY);
    let week_ago = now.saturating_sub(MILLIS_PER_WEEK);
    let month_ago = now.saturating_sub(MILLIS_PER_MONTH);

    let mut accums: BTreeMap<Vec<u8>, (Option<ckb_types::packed::Script>, PaymentAccum)> =
        BTreeMap::new();

    // Stream events in batches to avoid loading the entire month into memory at once.
    // Only accumulate events matching the requested event_type.
    let mut cursor: Option<PaymentHistoryCursor> = None;
    loop {
        let (batch, next_cursor) = store
            .query_payment_events(PaymentHistoryQuery {
                asset: AssetSelector::All,
                event_type: Some(event_type),
                start_time: month_ago,
                end_time: now,
                limit: REPORT_BATCH_SIZE,
                after: cursor,
            })
            .map_err(crate::rpc::utils::rpc_error_no_data)?;
        let done = next_cursor.is_none() || batch.is_empty();
        for event in &batch {
            let key = asset_key(&event.udt_type_script);
            let entry = accums
                .entry(key)
                .or_insert_with(|| (event.udt_type_script.clone(), PaymentAccum::default()));
            let accum = &mut entry.1;

            accum.monthly_amount_sum = accum.monthly_amount_sum.saturating_add(event.amount);
            accum.monthly_event_count = accum.monthly_event_count.saturating_add(1);
            if event.timestamp >= week_ago {
                accum.weekly_amount_sum = accum.weekly_amount_sum.saturating_add(event.amount);
                accum.weekly_event_count = accum.weekly_event_count.saturating_add(1);
            }
            if event.timestamp >= day_ago {
                accum.daily_amount_sum = accum.daily_amount_sum.saturating_add(event.amount);
                accum.daily_event_count = accum.daily_event_count.saturating_add(1);
            }
        }
        if done {
            break;
        }
        cursor = next_cursor;
    }

    let asset_reports = accums
        .into_values()
        .map(|(udt_type_script, accum)| AssetPaymentReport {
            udt_type_script: udt_type_script.map(Into::into),
            daily_amount_sum: accum.daily_amount_sum,
            weekly_amount_sum: accum.weekly_amount_sum,
            monthly_amount_sum: accum.monthly_amount_sum,
            daily_event_count: accum.daily_event_count,
            weekly_event_count: accum.weekly_event_count,
            monthly_event_count: accum.monthly_event_count,
        })
        .collect();

    Ok(asset_reports)
}

/// Core payment history logic.
pub fn payment_history_impl(
    store: &impl PaymentEventStore,
    params: PaymentHistoryParams,
) -> Result<PaymentHistoryResult, ErrorObjectOwned> {
    let now = now_timestamp_as_millis_u64();
    let start_time = params.start_time.unwrap_or(0);
    let end_time = params.end_time.unwrap_or(now);
    let limit = params.limit.unwrap_or(DEFAULT_PAYMENT_HISTORY_LIMIT) as usize;

    if limit > MAX_PAYMENT_HISTORY_LIMIT as usize {
        return Err(ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!(
                "limit exceeds maximum allowed value ({}). \
                Use a smaller limit or use start_time/end_time to narrow the time range.",
                MAX_PAYMENT_HISTORY_LIMIT
            ),
            Option::<()>::None,
        ));
    }

    let asset = payment_history_asset_selector(&params)?;
    let event_type = payment_history_event_type(params.event_type);
    let after_cursor = params
        .after
        .as_ref()
        .map(|bytes| PaymentHistoryCursor::from_bytes(bytes.as_bytes()))
        .transpose()
        .map_err(|err| ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err, Some(&params)))?;

    let query = PaymentHistoryQuery {
        asset,
        event_type,
        start_time,
        end_time,
        limit,
        after: after_cursor,
    };
    let (events, store_cursor) = store
        .query_payment_events(query)
        .map_err(|err| ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err, Some(&params)))?;

    let total_count = events.len() as u64;
    let last_cursor = store_cursor.map(|cursor| JsonBytes::from_vec(cursor.to_bytes()));
    let events = events
        .into_iter()
        .map(|e| PaymentEventInfo {
            event_type: match e.event_type {
                PaymentEventType::Send => "Send".to_string(),
                PaymentEventType::Receive => "Receive".to_string(),
            },
            timestamp: e.timestamp,
            channel_id: e.channel_id.into(),
            amount: e.amount,
            fee: e.fee,
            payment_hash: e.payment_hash.into(),
            udt_type_script: e.udt_type_script.map(Into::into),
        })
        .collect();

    Ok(PaymentHistoryResult {
        events,
        total_count,
        last_cursor,
    })
}

fn payment_history_asset_selector(
    params: &PaymentHistoryParams,
) -> Result<AssetSelector, ErrorObjectOwned> {
    if params.asset.is_some() && params.udt_type_script.is_some() {
        return Err(ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            "use either `asset` or deprecated `udt_type_script`, not both",
            Some(params),
        ));
    }

    let selector = match &params.asset {
        Some(PaymentHistoryAsset::Ckb) => AssetSelector::Ckb,
        Some(PaymentHistoryAsset::Udt { udt_type_script }) => {
            AssetSelector::Udt(udt_type_script.clone().into())
        }
        None => match &params.udt_type_script {
            Some(script) => AssetSelector::Udt(script.clone().into()),
            None => AssetSelector::All,
        },
    };

    Ok(selector)
}

fn payment_history_event_type(
    event_type: Option<PaymentHistoryEventType>,
) -> Option<PaymentEventType> {
    match event_type {
        Some(PaymentHistoryEventType::Send) => Some(PaymentEventType::Send),
        Some(PaymentHistoryEventType::Receive) => Some(PaymentEventType::Receive),
        None => None,
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

/// Accumulator for per-asset payment amount aggregation across time windows.
#[derive(Default)]
struct PaymentAccum {
    daily_amount_sum: u128,
    weekly_amount_sum: u128,
    monthly_amount_sum: u128,
    daily_event_count: u64,
    weekly_event_count: u64,
    monthly_event_count: u64,
}

/// Key for grouping: serialized `Option<Script>` bytes.
/// We use `Vec<u8>` so we can use BTreeMap for deterministic ordering.
fn asset_key(udt_type_script: &Option<ckb_types::packed::Script>) -> Vec<u8> {
    match udt_type_script {
        None => vec![0],
        Some(script) => {
            let mut key = vec![1];
            key.extend_from_slice(script.as_slice());
            key
        }
    }
}
