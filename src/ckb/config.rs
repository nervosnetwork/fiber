
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};

#[derive(ClapSerde, Debug)]
pub struct CkbConfig {
    #[arg(short, long, env = "NAME", help = "Your name")]
    pub name: String,

    #[arg(short, long)]
    pub age: u8,
}
