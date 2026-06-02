use std::path::PathBuf;

use clap_serde_derive::ClapSerde;
use serde::{Deserialize, Serialize};

/// Identity of a Fiber-side asset accepted by the CCH.
///
/// `None` denotes the native CKB asset; `Some(script)` denotes a UDT identified
/// by its full type script. This mirrors how Fiber invoices encode the asset
/// (no `udt_script` attribute = native CKB, otherwise the UDT type script).
pub type CchAsset = Option<ckb_jsonrpc_types::Script>;

/// A Fiber asset for which the hub publishes a fixed exchange rate against
/// satoshis, allowing the fast-path swap flow to mint the counterparty
/// invoice immediately without operator intervention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedRateAsset {
    /// The Fiber asset this rate applies to. `None` = native CKB.
    #[serde(default)]
    pub fiber_asset: CchAsset,
    /// Exchange rate expressed as the number of Fiber-asset smallest units
    /// (shannon for native CKB, or the UDT's smallest denomination) that one
    /// satoshi buys. Must be > 0.
    ///
    /// Conversion formulas (Fiber-leg amount denominated in smallest units,
    /// BTC-leg amount denominated in millisatoshi):
    ///
    /// * SendBTC fast path:    `fiber = btc_msat * rate / 1000`
    /// * ReceiveBTC fast path: `btc_msat = fiber * 1000 / rate`
    ///
    /// A wrapped-BTC-style 1:1 mapping uses `rate = 1` (1 sat = 1 smallest
    /// unit). Higher `rate` means MORE smallest units per sat, i.e. the
    /// Fiber asset is LESS valuable per unit than BTC.
    pub smallest_units_per_sat: u128,
}

/// Default cross-chain order relative expiry time in seconds.
pub const DEFAULT_ORDER_EXPIRY_DELTA_SECONDS: u64 = 36 * 60 * 60; // 36 hours
/// Default BTC final-hop HTLC expiry time in blocks.
pub const DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS: u64 = 360; // 60 hours (~10 min/block)
/// Default CKB final-hop HTLC expiry delta in seconds.
pub const DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS: u64 = 60 * 60 * 60; // 60 hours
/// Default minimum outgoing invoice relative expiry time in seconds.
pub const DEFAULT_MIN_OUTGOING_INVOICE_EXPIRY_DELTA_SECONDS: u64 = 6 * 60 * 60; // 6 hours
/// Default percentage of the collected CCH fee that may be spent on outgoing routing fees.
pub const DEFAULT_MAX_OUTGOING_FEE_PERCENTAGE: u64 = 100;

// Use prefix `cch-`/`CCH_`
#[derive(ClapSerde, Debug, Clone)]
pub struct CchConfig {
    /// cch base directory
    #[arg(
        name = "CCH_BASE_DIR",
        long = "cch-base-dir",
        env,
        help = "base directory for cch [default: $BASE_DIR/cch]"
    )]
    pub base_dir: Option<PathBuf>,

    #[default("https://127.0.0.1:10009".to_string())]
    #[arg(
        name = "CCH_LND_RPC_URL",
        long = "cch-lnd-rpc-url",
        env,
        help = "lnd grpc endpoint, default is http://127.0.0.1:10009"
    )]
    pub lnd_rpc_url: String,

    #[arg(
        name = "CCH_LND_CERT_PATH",
        long = "cch-lnd-cert-path",
        env,
        help = "Path to the TLS cert file for the grpc connection. Leave it empty to use wellknown CA certificates like Let's Encrypt."
    )]
    pub lnd_cert_path: Option<String>,

    #[arg(
        name = "CCH_LND_MACAROON_PATH",
        long = "cch-lnd-macaroon-path",
        env,
        help = "Path to the Macaroon file for the grpc connection"
    )]
    pub lnd_macaroon_path: Option<String>,

    /// Fiber assets the hub is willing to swap against BTC. Each entry is an
    /// optional CKB type script: `null`/absent denotes native CKB, otherwise
    /// the full UDT type script identifies the asset.
    /// Only assets present in this list are accepted by `send_btc` and
    /// `receive_btc`.
    #[arg(skip)]
    #[serde(default)]
    pub fiber_asset_allowlist: Vec<CchAsset>,

    /// Subset of allowlisted Fiber assets for which the hub publishes a fixed
    /// satoshi-per-smallest-unit exchange rate. Orders involving these assets
    /// take the fast path: the hub computes the counterparty leg amount from
    /// the configured rate plus fees and accepts the order without any
    /// out-of-band proposal step.
    #[arg(skip)]
    #[serde(default)]
    pub fixed_rate_assets: Vec<FixedRateAsset>,

    /// Timeout, in seconds, that the hub waits for an operator response to a
    /// `SwapProposal` (proposal-path swaps only — see the swap acceptor
    /// protocol). When the timeout elapses without a response, the proposal
    /// is treated as rejected and the originating `send_btc` / `receive_btc`
    /// call returns an error.
    #[default(30)]
    #[arg(
        name = "CCH_SWAP_PROPOSAL_TIMEOUT_SECONDS",
        long = "cch-swap-proposal-timeout-seconds",
        env,
        help = "Seconds to wait for an operator SwapProposalResponse before auto-rejecting (default: 30)."
    )]
    pub swap_proposal_timeout_seconds: u64,

    /// Cross-chain order expiry time in seconds.
    #[default(DEFAULT_ORDER_EXPIRY_DELTA_SECONDS)]
    #[arg(
        name = "CCH_ORDER_EXPIRY_DELTA_SECONDS",
        long = "cch-order-expiry-delta-seconds",
        env,
        help = format!("order relative expiry time in seconds, default is {}", DEFAULT_ORDER_EXPIRY_DELTA_SECONDS),
    )]
    pub order_expiry_delta_seconds: u64,

    #[default(0)]
    #[arg(
        name = "CCH_BASE_FEE_SATS",
        long = "cch-base-fee-sats",
        env,
        help = "The base fee charged for each cross-chain order, default is 0"
    )]
    pub base_fee_sats: u64,

    #[default(1)]
    #[arg(
        name = "CCH_FEE_RATE_PER_MILLION_SATS",
        long = "cch-fee-rate-per-million-sats",
        env,
        help = "The proportional fee charged per million satoshis based on the cross-chain order value, default is 1"
    )]
    pub fee_rate_per_million_sats: u64,

    /// Maximum percentage of the collected CCH fee that may be used as the routing
    /// fee budget for the outgoing payment leg.
    ///
    /// The outgoing payment fee is capped at `fee_sats * max_outgoing_fee_percentage / 100`,
    /// where `fee_sats` is the fee charged on the incoming/order leg. This guarantees the
    /// outgoing route fee never exceeds the fee the operator collected. Defaults to 100,
    /// i.e. the entire collected fee may be spent on the outgoing route.
    #[default(DEFAULT_MAX_OUTGOING_FEE_PERCENTAGE)]
    #[arg(
        name = "CCH_MAX_OUTGOING_FEE_PERCENTAGE",
        long = "cch-max-outgoing-fee-percentage",
        env,
        help = format!("Maximum percentage of the collected CCH fee usable as the outgoing payment routing fee budget (1-100), default is {}", DEFAULT_MAX_OUTGOING_FEE_PERCENTAGE),
    )]
    pub max_outgoing_fee_percentage: u64,

    /// Final tlc expiry time for BTC network.
    #[default(DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS)]
    #[arg(
        name = "CCH_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS",
        long = "cch-btc-final-tlc-expiry-delta-blocks",
        env,
        help = format!("final tlc relative expiry time in blocks for BTC network, default is {}", DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS),
    )]
    pub btc_final_tlc_expiry_delta_blocks: u64,

    /// Tlc expiry time for CKB network in blocks.
    #[default(DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS)]
    #[arg(
        name = "CCH_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS",
        long = "cch-ckb-final-tlc-expiry-delta-seconds",
        env,
        help = format!("final tlc relative expiry time in seconds for CKB network, default is {}", DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS),
    )]
    pub ckb_final_tlc_expiry_delta_seconds: u64,

    /// Minimum acceptable relative expiry time in seconds for the cch outgoing invoice.
    #[default(DEFAULT_MIN_OUTGOING_INVOICE_EXPIRY_DELTA_SECONDS)]
    #[arg(
        name = "CCH_MIN_OUTGOING_INVOICE_EXPIRY_DELTA_SECONDS",
        long = "cch-min-outgoing-invoice-expiry-delta-seconds",
        env,
        help = format!("minimum acceptable relative expiry time in seconds for the cch outgoing invoice, default is {}", DEFAULT_MIN_OUTGOING_INVOICE_EXPIRY_DELTA_SECONDS),
    )]
    pub min_outgoing_invoice_expiry_delta_seconds: u64,

    /// Ignore the failure when starting the cch service.
    #[default(false)]
    #[arg(skip)]
    pub ignore_startup_failure: bool,

    /// Fiber RPC endpoint for connecting to an external Fiber node.
    /// When set, CCH runs as a separate service and communicates with the Fiber node
    /// via HTTP RPC and WebSocket subscriptions.
    /// The address format should be http[s]://<host>:<port>.
    /// If http is specified, the WebSocket connection will be ws://<host>:<port>;
    /// if https is specified, the WebSocket connection will be wss://<host>:<port>.
    #[default(None)]
    #[arg(
        name = "CCH_FIBER_RPC_URL",
        long = "cch-fiber-rpc-url",
        env,
        help = "fiber endpoint, default is None. May be used to connect to an external fiber node with websocket and normal http jsonrpc support."
    )]
    pub fiber_rpc_url: Option<String>,
}

impl CchConfig {
    /// Validate that the config is usable for standalone mode (no Fiber/CKB services).
    ///
    /// In standalone mode the contracts context is never initialized, so every
    /// allowlisted UDT must already be specified via its full type script
    /// (which is the only way to declare assets in the new config schema).
    /// We therefore only need to check that at least one asset is allowlisted.
    /// Returns `Ok(())` on success, or an error string describing the problem.
    pub fn validate_standalone(&self) -> Result<(), String> {
        if self.fiber_asset_allowlist.is_empty() {
            return Err(
                "CCH standalone mode requires `fiber_asset_allowlist` to contain \
                 at least one asset (native CKB as `null`, or a UDT type script)"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Validate config values that must hold regardless of run mode.
    ///
    /// Checks that:
    /// - `fixed_rate_assets` do not have `smallest_units_per_sat = 0`, because
    ///   the fast-path conversion formulas use it as a divisor and multiplier
    ///   (see [`FixedRateAsset`]).
    /// - `max_outgoing_fee_percentage` is within the valid `1..=100` range so
    ///   the outgoing payment fee budget is always a sane fraction of the
    ///   collected CCH fee.
    pub fn validate(&self) -> Result<(), String> {
        for entry in &self.fixed_rate_assets {
            if entry.smallest_units_per_sat == 0 {
                return Err(format!(
                    "fixed-rate asset {:?} has smallest_units_per_sat = 0; \
                     must be a positive integer (see FixedRateAsset docs)",
                    entry.fiber_asset
                ));
            }
        }
        if !(1..=100).contains(&self.max_outgoing_fee_percentage) {
            return Err(format!(
                "`max_outgoing_fee_percentage` must be between 1 and 100, got {}",
                self.max_outgoing_fee_percentage
            ));
        }
        Ok(())
    }

    pub fn resolve_lnd_cert_path(&self) -> Option<PathBuf> {
        self.lnd_cert_path.as_ref().map(|lnd_cert_path| {
            let path = PathBuf::from(lnd_cert_path);
            match (self.base_dir.clone(), path.is_relative()) {
                (Some(base_dir), true) => base_dir.join(path),
                _ => path,
            }
        })
    }

    pub fn resolve_lnd_macaroon_path(&self) -> Option<PathBuf> {
        self.lnd_macaroon_path.as_ref().map(|lnd_macaroon_path| {
            let path = PathBuf::from(lnd_macaroon_path);
            match (self.base_dir.clone(), path.is_relative()) {
                (Some(base_dir), true) => base_dir.join(path),
                _ => path,
            }
        })
    }
}
