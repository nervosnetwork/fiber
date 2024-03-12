use std::{fs::File, io::BufReader, process::exit};

use clap::CommandFactory;
use clap_serde_derive::{
    clap::{self, Parser},
    ClapSerde,
};
use serde::Deserialize;

use crate::LdkConfig;

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Config file
    #[arg(short, long = "config", default_value = "config.yml")]
    config_path: std::path::PathBuf,

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

pub struct Config {
    pub ckb: CkbConfig,
    pub ldk: LdkConfig,
}

#[derive(ClapSerde)]
pub struct CkbConfig {
    #[arg(short, long, env = "NAME", help = "Your name")]
    pub name: String,

    #[arg(short, long)]
    pub age: u8,
}

pub fn print_help_and_exit(code: i32) {
    if atty::is(atty::Stream::Stdin) {
        let mut cmd = Args::command();
        cmd.print_help().expect("print help");
        exit(code);
    }
}

impl Config {
    pub fn parse() -> Self {
        // Parse whole args with clap
        let mut args = Args::parse();

        let config_from_file = serde_yaml::from_reader::<_, SerializedConfig>(BufReader::new(
            File::open(&args.config_path).expect("valid config file"),
        ))
        .expect("valid config file format");

        let ckb = match config_from_file.ckb {
            Some(config) => CkbConfig::from(config).merge(&mut args.ckb),
            _ => CkbConfig::from(&mut args.ckb),
        };
        let ldk = match config_from_file.ldk {
            Some(config) => LdkConfig::from(config).merge(&mut args.ldk),
            _ => LdkConfig::from(&mut args.ldk),
        };
        Self { ckb, ldk }
    }
}
