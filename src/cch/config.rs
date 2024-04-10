use std::path::PathBuf;

use clap_serde_derive::ClapSerde;

/// Default cross-chain order expiry time in seconds.
pub const DEFAULT_ORDER_EXPIRY_TIME: u64 = 3600;
/// Default BTC final-hop HTLC expiry time in seconds.
/// CCH will only use one-hop payment in CKB network.
pub const DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME: u64 = 36;
/// Default CKB final-hop HTLC expiry time in seconds.
/// Leave enough time for routing the BTC payment
pub const DEFAULT_CKB_FINAL_TLC_EXPIRY_TIME: u64 = 108;

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
    pub lnd_macaroon_path: String,

    #[arg(
        name = "CCH_RATIO_BTC_MSAT",
        long = "cch-ratio-btc-msat",
        env,
        help = "exchange ratio between BTC and CKB, in milisatoshi per `CCH_RATIO_CKB_SHANNONS` shannon"
    )]
    pub ratio_btc_msat: Option<u64>,
    #[arg(
        name = "CCH_RATIO_CKB_SHANNONS",
        long = "cch-ratio-ckb-shannons",
        env,
        help = "exchange ratio between BTC and CKB, in shannons per `CCH_RATIO_BTC_MSAT` shannon"
    )]
    pub ratio_ckb_shannons: Option<u64>,

    /// Whether reject expired BTC invoice when creating the order to send BTC.
    ///
    /// Default is `false`. Only set to `true` in test.
    #[default(false)]
    #[arg(skip)]
    pub allow_expired_btc_invoice: bool,

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
        name = "CCH_BASE_FEE_SHANNONS",
        long = "cch-base-fee-shannons",
        env,
        help = "The base fee charged for each cross-chain order, default is 0"
    )]
    pub base_fee_shannons: u64,

    #[default(1)]
    #[arg(
        name = "CCH_FEE_RATE_PER_MILLION_SHANNONS",
        long = "cch-fee-rate-per-million-shannons",
        env,
        help = "The proportional fee charged per million shannons based on the cross-chain order value, default is 1"
    )]
    pub fee_rate_per_million_shannons: u64,

    /// Final tlc expiry time for BTC network.
    #[default(DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME)]
    #[arg(
        name = "CCH_BTC_FINAL_TLC_EXPIRY",
        long = "cch-btc-final-tlc-expiry",
        env,
        help = format!("final tlc expiry time in seconds for BTC network, default is {}", DEFAULT_BTC_FINAL_TLC_EXPIRY_TIME),
    )]
    pub btc_final_tlc_expiry: u64,

    /// Final tlc expiry time for CKB network.
    #[default(DEFAULT_CKB_FINAL_TLC_EXPIRY_TIME)]
    #[arg(
        name = "CCH_CKB_FINAL_TLC_EXPIRY",
        long = "cch-ckb-final-tlc-expiry",
        env,
        help = format!("final tlc expiry time in seconds for CKB network, default is {}", DEFAULT_CKB_FINAL_TLC_EXPIRY_TIME),
    )]
    pub ckb_final_tlc_expiry: u64,
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

    pub fn resolve_lnd_macaroon_path(&self) -> PathBuf {
        let path = PathBuf::from(&self.lnd_macaroon_path);
        match (self.base_dir.clone(), path.is_relative()) {
            (Some(base_dir), true) => base_dir.join(path),
            _ => path,
        }
    }
}
