use std::path::PathBuf;

use clap_serde_derive::ClapSerde;

/// Default cross-chain order expiry time in seconds.
pub const DEFAULT_ORDER_EXPIRY_TIME: u64 = 3600;
/// Default BTC final-hop HTLC expiry time in seconds.
pub const DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME: u64 = 36;
/// Default CKB final-hop HTLC expiry time in blocks.
pub const DEFAULT_CKB_FINAL_TLC_EXPIRY_BLOCKS: u64 = 10;

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
    #[default(DEFAULT_ORDER_EXPIRY_TIME)]
    #[arg(
        name = "CCH_ORDER_EXPIRY",
        long = "cch-order-expiry",
        env,
        help = format!("order expiry time in seconds, default is {}", DEFAULT_ORDER_EXPIRY_TIME),
    )]
    pub order_expiry: u64,

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
    #[default(DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME)]
    #[arg(
        name = "CCH_BTC_FINAL_TLC_EXPIRY",
        long = "cch-btc-final-tlc-expiry",
        env,
        help = format!("final tlc expiry time in seconds for BTC network, default is {}", DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME),
    )]
    pub btc_final_tlc_expiry: u64,

    /// Final tlc expiry time for CKB network in blocks.
    #[default(DEFAULT_CKB_FINAL_TLC_EXPIRY_BLOCKS)]
    #[arg(
        name = "CCH_CKB_FINAL_TLC_EXPIRY_BLOCKS",
        long = "cch-ckb-final-tlc-expiry-blocks",
        env,
        help = format!("final tlc expiry time in blocks for CKB network, default is {}", DEFAULT_CKB_FINAL_TLC_EXPIRY_BLOCKS),
    )]
    pub ckb_final_tlc_expiry_blocks: u64,

    /// Ignore the failure when starting the cch service.
    #[default(false)]
    pub ignore_startup_failure: bool,
}

impl CchConfig {
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
