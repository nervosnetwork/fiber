use serde::Deserialize;
use serde_with::serde_as;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum CchCommand {
    SendBTC(SendBTC),
}

impl CchCommand {
    pub fn name(&self) -> &'static str {
        match self {
            CchCommand::SendBTC(_) => "SendBTC",
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
}
