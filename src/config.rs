use std::{fs::File, io::BufReader, path::PathBuf, process::exit, str::FromStr};

use clap::CommandFactory;
use clap_serde_derive::{
    clap::{self, Parser},
    ClapSerde,
};
use home::home_dir;
use log::error;
use serde::{Deserialize, Serialize};

use crate::{CkbConfig, LdkConfig};

fn get_base_dir() -> PathBuf {
    let mut path = home_dir().expect("get home directory");
    path.push(".ckb-pcn-node");
    path
}

const DEFAULT_CONFIG_FILE_NAME: &str = "config.yml";
const DEFAULT_CKB_DIR_NAME: &str = "ckb";
const DEFAULT_LDK_DIR_NAME: &str = "ldk";

fn get_default_config_file() -> PathBuf {
    let mut path = get_base_dir();
    path.push(DEFAULT_CONFIG_FILE_NAME);
    path
}

#[derive(Serialize, Deserialize, Parser, Copy, Clone, Debug, PartialEq)]
enum Service {
    #[serde(alias = "ckb", alias = "CKB")]
    CKB,
    #[serde(alias = "ldk", alias = "ldk")]
    LDK,
}

impl FromStr for Service {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ckb" | "CKB" => Ok(Self::CKB),
            "ldk" | "LDK" => Ok(Self::LDK),
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
    config_path: Option<std::path::PathBuf>,

    /// base directory
    #[arg(short = 'd', long = "dir", help = format!("base directory for all [default: {:?}]", get_base_dir()))]
    base_dir: Option<std::path::PathBuf>,

    /// services to run (can be any of `ckb`/`ldk`, separated by `,`)
    #[arg(short, long, value_parser, num_args = 0.., value_delimiter = ',')]
    services: Vec<Service>,

    /// config for ckb (payment channel network over ckb)
    #[command(flatten)]
    pub ckb: <CkbConfig as ClapSerde>::Opt,

    /// config for ldk (lightning network for bitcoin)
    #[command(flatten)]
    pub ldk: <LdkConfig as ClapSerde>::Opt,
}

#[derive(Deserialize)]
struct SerializedConfig {
    services: Option<Vec<Service>>,
    ckb: Option<<CkbConfig as ClapSerde>::Opt>,
    ldk: Option<<LdkConfig as ClapSerde>::Opt>,
}

#[derive(Debug)]
pub struct Config {
    pub ckb: Option<CkbConfig>,
    pub ldk: Option<LdkConfig>,
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

        let base_dir = args.base_dir.clone().unwrap_or(get_base_dir());

        let config_file = args
            .config_path
            .or(args.base_dir.map(|x| x.join(DEFAULT_CONFIG_FILE_NAME)))
            .unwrap_or(get_default_config_file());

        let config_from_file = File::open(&config_file).map(BufReader::new).map(|f| {
            serde_yaml::from_reader::<_, SerializedConfig>(f).expect("valid config file format")
        });

        let services = if args.services.is_empty() {
            config_from_file
                .as_ref()
                .ok()
                .and_then(|x| x.services.clone())
                .unwrap_or(vec![])
        } else {
            args.services
        };
        if services.is_empty() {
            error!("Must run at least one service");
            print_help_and_exit(1)
        };

        args.ckb.base_dir = Some(Some(base_dir.join(DEFAULT_CKB_DIR_NAME)));
        args.ldk.base_dir = Some(Some(base_dir.join(DEFAULT_LDK_DIR_NAME)));

        let (ckb, ldk) = match config_from_file
            .map(|x| match x {
                SerializedConfig {
                    services: _,
                    ckb,
                    ldk,
                } => (
                    ckb.map(|c| CkbConfig::from(c).merge(&mut args.ckb)),
                    ldk.map(|c| LdkConfig::from(c).merge(&mut args.ldk)),
                ),
            })
            .unwrap_or((None, None))
        {
            (ckb, ldk) => (
                ckb.unwrap_or(CkbConfig::from(&mut args.ckb)),
                ldk.unwrap_or(LdkConfig::from(&mut args.ldk)),
            ),
        };
        let ckb = if services.contains(&Service::CKB) {
            Some(ckb)
        } else {
            None
        };
        let ldk = if services.contains(&Service::LDK) {
            Some(ldk)
        } else {
            None
        };
        Self { ckb, ldk }
    }
}
