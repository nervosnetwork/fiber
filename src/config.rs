use std::{fs::File, io::BufReader, path::PathBuf, process::exit, str::FromStr};

use clap::CommandFactory;
use clap_serde_derive::{
    clap::{self, Parser},
    ClapSerde,
};
use home::home_dir;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{ckb_chain::CkbChainConfig, CchConfig, CkbConfig, LdkConfig, RpcConfig};

const DEFAULT_CONFIG_FILE_NAME: &str = "config.yml";
const DEFAULT_CKB_DIR_NAME: &str = "ckb";
const DEFAULT_LDK_DIR_NAME: &str = "ldk";

fn get_default_base_dir() -> PathBuf {
    let mut path = home_dir().expect("get home directory");
    path.push(".cfn-node");
    path
}

fn get_default_config_file() -> PathBuf {
    let mut path = get_default_base_dir();
    path.push(DEFAULT_CONFIG_FILE_NAME);
    path
}

#[derive(Serialize, Deserialize, Parser, Copy, Clone, Debug, PartialEq)]
enum Service {
    #[serde(alias = "ckb", alias = "CKB")]
    CKB,
    #[serde(alias = "ldk", alias = "LDK")]
    LDK,
    #[serde(alias = "cch", alias = "CCH")]
    CCH,
    #[serde(alias = "rpc", alias = "RPC")]
    RPC,
    #[serde(alias = "ckb_chain", alias = "CKB_CHAIN")]
    CkbChain,
}

impl FromStr for Service {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ckb" | "CKB" => Ok(Self::CKB),
            "ldk" | "LDK" => Ok(Self::LDK),
            "cch" | "CCH" => Ok(Self::CCH),
            "rpc" | "RPC" => Ok(Self::RPC),
            "ckb_chain" | "CKB_CHAIN" => Ok(Self::CkbChain),
            _ => Err(format!("invalid service {}", s)),
        }
    }
}

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    // We want to differentiate between when it is a user-set value or it is the default value.
    // If the user has not set default value but set `base_dir` instead then we will use `config.yml`,
    // under base dir.
    /// config file
    #[arg(short, long = "config", help = format!("config file [default: {:?} or $BASE_DIR/config.yml]", get_default_config_file()))]
    config_file: Option<std::path::PathBuf>,

    /// base directory
    #[arg(short = 'd', long = "dir", help = format!("base directory for all [default: {:?}]", get_default_base_dir()))]
    base_dir: Option<std::path::PathBuf>,

    /// services to run (can be any of `ckb`/`ldk`, separated by `,`)
    #[arg(short, long, value_parser, num_args = 0.., value_delimiter = ',')]
    services: Vec<Service>,

    /// config for ckb fiber network
    #[command(flatten)]
    pub ckb: <CkbConfig as ClapSerde>::Opt,

    /// config for ldk (lightning network for bitcoin)
    #[command(flatten)]
    pub ldk: <LdkConfig as ClapSerde>::Opt,

    /// config for cch (cross chain hub)
    #[command(flatten)]
    pub cch: <CchConfig as ClapSerde>::Opt,

    /// config for rpc
    #[command(flatten)]
    pub rpc: <RpcConfig as ClapSerde>::Opt,

    /// config for ckb chain
    #[command(flatten)]
    pub ckb_chain: <CkbChainConfig as ClapSerde>::Opt,
}

#[derive(Deserialize)]
struct SerializedConfig {
    services: Option<Vec<Service>>,
    ckb: Option<<CkbConfig as ClapSerde>::Opt>,
    ldk: Option<<LdkConfig as ClapSerde>::Opt>,
    cch: Option<<CchConfig as ClapSerde>::Opt>,
    rpc: Option<<RpcConfig as ClapSerde>::Opt>,
    ckb_chain: Option<<CkbChainConfig as ClapSerde>::Opt>,
}

#[derive(Debug)]
pub struct Config {
    // ckb config, None represents that we should not run ckb service
    pub ckb: Option<CkbConfig>,
    // ldk config, None represents that we should not run ldk service
    pub ldk: Option<LdkConfig>,
    // cch config, None represents that we should not run cch service
    pub cch: Option<CchConfig>,
    // rpc server config, None represents that we should not run rpc service
    pub rpc: Option<RpcConfig>,
    // ckb chain actor config, None represents that we should not run ckb chain actor
    pub ckb_chain: Option<CkbChainConfig>,
}

pub(crate) fn print_help_and_exit(code: i32) {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        let mut cmd = Args::command();
        cmd.print_help().expect("print help");
    }
    exit(code);
}

impl Config {
    pub fn parse() -> Self {
        // Parse whole args with clap
        let mut args = Args::parse();

        // Base directory for all things to be stored to disk
        let base_dir = args.base_dir.clone().unwrap_or(get_default_base_dir());

        // Get config file by
        // 1. Using the explicitly set command line argument `config`
        // 2. Prepending `config.yml` to the explicitly set command line argument `dir`
        // 3. Using the default `config.yml` file
        let config_file = args
            .config_file
            .or(args.base_dir.map(|x| x.join(DEFAULT_CONFIG_FILE_NAME)))
            .unwrap_or(get_default_config_file());

        let config_from_file = File::open(config_file).map(BufReader::new).map(|f| {
            serde_yaml::from_reader::<_, SerializedConfig>(f).expect("valid config file format")
        });

        // Services to run can be passed from
        // 1. command line
        // 2. config file
        // If command line arguments contain services, then don't read config file
        // for services to run any more, otherwise use config file for that.
        let services = if args.services.is_empty() {
            config_from_file
                .as_ref()
                .ok()
                .and_then(|x| x.services.clone())
                .unwrap_or_default()
        } else {
            args.services
        };

        if services.is_empty() {
            error!("Must run at least one service. Specifying services to run by command line or config file.");
            print_help_and_exit(1)
        };

        // Set default ckb/ldk base directory. These may be overridden by values explicitly set by the user.
        args.ckb.base_dir = Some(Some(base_dir.join(DEFAULT_CKB_DIR_NAME)));
        args.ldk.base_dir = Some(Some(base_dir.join(DEFAULT_LDK_DIR_NAME)));
        args.ckb_chain.base_dir = Some(Some(
            base_dir.join(crate::ckb_chain::DEFAULT_CKB_CHAIN_BASE_DIR_NAME),
        ));

        let (ckb, ldk, cch, rpc, ckb_chain) = config_from_file
            .map(|x| {
                let SerializedConfig {
                    services: _,
                    ckb,
                    ldk,
                    cch,
                    rpc,
                    ckb_chain,
                } = x;
                (
                    // Successfully read config file, merging these options with the default ones.
                    ckb.map(|c| CkbConfig::from(c).merge(&mut args.ckb)),
                    ldk.map(|c| LdkConfig::from(c).merge(&mut args.ldk)),
                    cch.map(|c| CchConfig::from(c).merge(&mut args.cch)),
                    rpc.map(|c| RpcConfig::from(c).merge(&mut args.rpc)),
                    ckb_chain.map(|c| CkbChainConfig::from(c).merge(&mut args.ckb_chain)),
                )
            })
            .unwrap_or((None, None, None, None, None));
        let (ckb, ldk, cch, rpc, ckb_chain) = (
            ckb.unwrap_or(CkbConfig::from(&mut args.ckb)),
            ldk.unwrap_or(LdkConfig::from(&mut args.ldk)),
            cch.unwrap_or(CchConfig::from(&mut args.cch)),
            rpc.unwrap_or(RpcConfig::from(&mut args.rpc)),
            ckb_chain.unwrap_or(CkbChainConfig::from(&mut args.ckb_chain)),
        );

        let ckb = services.contains(&Service::CKB).then_some(ckb);
        let ldk = services.contains(&Service::LDK).then_some(ldk);
        let cch = services.contains(&Service::CCH).then_some(cch);
        let rpc = services.contains(&Service::RPC).then_some(rpc);
        let ckb_chain = services.contains(&Service::CkbChain).then_some(ckb_chain);
        Self {
            ckb,
            ldk,
            cch,
            rpc,
            ckb_chain,
        }
    }
}
