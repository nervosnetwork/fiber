use std::path::PathBuf;

use clap_serde_derive::ClapSerde;

/// Default cross-chain order relative expiry time in seconds.
pub const DEFAULT_ORDER_EXPIRY_DELTA_SECONDS: u64 = 36 * 60 * 60; // 36 hours
/// Default BTC final-hop HTLC expiry time in blocks.
pub const DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS: u64 = 180; // 30 hours
/// Default CKB final-hop HTLC expiry delta in seconds.
pub const DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS: u64 = 30 * 60 * 60; // 30 hours
/// Default minimum outgoing invoice relative expiry time in seconds.
pub const DEFAULT_MIN_OUTGOING_INVOICE_EXPIRY_DELTA_SECONDS: u64 = 6 * 60 * 60; // 6 hours

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

    // TODO: use hex type
    #[arg(
        name = "CCH_WRAPPED_BTC_TYPE_SCRIPT_ARGS",
        long = "cch-wrapped-btc-type-script-args",
        env,
        help = "Wrapped BTC type script args. It must be a UDT with 8 decimal places."
    )]
    pub wrapped_btc_type_script_args: String,

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

    /// Full wrapped BTC type script as a JSON object, e.g.
    /// `{"code_hash":"0x...", "hash_type":"type", "args":"0x..."}`.
    /// Required when running CCH in standalone mode (without the Fiber/CKB services)
    /// because the contracts context is not initialized.
    /// When set, this takes precedence over constructing the script from the
    /// contracts context with `wrapped_btc_type_script_args`.
    #[default(None)]
    #[arg(
        name = "CCH_WRAPPED_BTC_TYPE_SCRIPT",
        long = "cch-wrapped-btc-type-script",
        env,
        help = "Full wrapped BTC type script as JSON. Required in standalone mode where the contracts context is unavailable."
    )]
    pub wrapped_btc_type_script: Option<String>,
}

impl CchConfig {
    /// Validate that the config is usable for standalone mode (no Fiber/CKB services).
    ///
    /// In standalone mode the contracts context is never initialized, so the
    /// full wrapped BTC type script must be provided explicitly.
    /// Returns `Ok(())` on success, or an error string describing the problem.
    pub fn validate_standalone(&self) -> Result<(), String> {
        match &self.wrapped_btc_type_script {
            None => Err(
                "CCH standalone mode requires `wrapped_btc_type_script` to be set \
                 because the contracts context is not available"
                    .to_string(),
            ),
            Some(json_str) => {
                serde_json::from_str::<ckb_jsonrpc_types::Script>(json_str).map_err(|e| {
                    format!(
                        "failed to parse `wrapped_btc_type_script` config as Script JSON: {}",
                        e
                    )
                })?;
                Ok(())
            }
        }
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
