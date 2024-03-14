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

fn get_default_config_file() -> PathBuf {
    let mut path = get_base_dir();
    path.push("config.yml");
    path
}

pub fn get_default_ldk_dir() -> PathBuf {
    let mut path = get_base_dir();
    path.push("ldk");
    path
}

pub fn get_default_ckb_dir() -> PathBuf {
    let mut path = get_base_dir();
    path.push("ckb");
    path
}

#[derive(Serialize, Deserialize, Parser, Copy, Clone, Debug, PartialEq)]
enum Service {
    CKB,
    LDK,
}

impl FromStr for Service {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "ckb" => Ok(Self::CKB),
            "ldk" => Ok(Self::LDK),
            _ => Err(format!("invalid service {}", s)),
        }
    }
}

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Config file
    #[arg(short, long = "config", default_value=get_default_config_file().into_os_string())]
    config_path: std::path::PathBuf,

    /// Config fileget_default_config_file
    #[arg(short, long, value_parser, num_args = 0.., value_delimiter = ',')]
    services: Vec<Service>,

    /// Rest of arguments
    #[command(flatten)]
    pub ckb: <CkbConfig as ClapSerde>::Opt,

    /// Rest of arguments
    #[command(flatten)]
    pub ldk: <LdkConfig as ClapSerde>::Opt,
}

#[derive(Deserialize)]
struct SerializedConfig {
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

        if args.services.is_empty() {
            error!("Must run at least one service");
            print_help_and_exit(1)
        }

        let config_from_file = serde_yaml::from_reader::<_, SerializedConfig>(BufReader::new(
            File::open(&args.config_path).expect("valid config file"),
        ))
        .expect("valid config file format");

        let ckb =
            args.services.iter().find(|x| **x == Service::CKB).map(|_| {
                match config_from_file.ckb {
                    Some(config) => CkbConfig::from(config).merge(&mut args.ckb),
                    _ => CkbConfig::from(&mut args.ckb),
                }
            });
        let ldk =
            args.services.iter().find(|x| **x == Service::LDK).map(|_| {
                match config_from_file.ldk {
                    Some(config) => LdkConfig::from(config).merge(&mut args.ldk),
                    _ => LdkConfig::from(&mut args.ldk),
                }
            });
        Self { ckb, ldk }
    }
}
