use std::{fs::File, io::BufReader, process::exit};

use clap::CommandFactory;
use clap_serde_derive::{
    clap::{self, Parser},
    ClapSerde,
};

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Config file
    #[arg(short, long = "config", default_value = "config.yml")]
    config_path: std::path::PathBuf,

    /// Rest of arguments
    #[command(flatten)]
    pub config: <Config as ClapSerde>::Opt,
}

#[derive(ClapSerde)]
pub struct Config {
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

        // Get config file
        let config = if let Ok(f) = File::open(&args.config_path) {
            // Parse config with serde
            match serde_yaml::from_reader::<_, <Config as ClapSerde>::Opt>(BufReader::new(f)) {
                // merge config already parsed from clap
                Ok(config) => Config::from(config).merge(&mut args.config),
                Err(err) => panic!("Error in configuration file:\n{}", err),
            }
        } else {
            // If there is not config file return only config parsed from clap
            Config::from(&mut args.config)
        };
        config
    }
}
