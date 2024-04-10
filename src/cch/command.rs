use serde::Deserialize;
use serde_with::serde_as;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum CchCommand {
    SendBTC(SendBTC),
    // TODO(cch): Delete this test RPC, and subscribe to CKB HTLC events to trigger the cross-chain
    // order payment.
    TestPayBTC(TestPayBTC),
}

impl CchCommand {
    pub fn name(&self) -> &'static str {
        match self {
            CchCommand::SendBTC(_) => "SendBTC",
            CchCommand::TestPayBTC(_) => "TestPayBTC",
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
}

/// Test fulfilling the SendBTC order.
#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct TestPayBTC {
    pub payment_hash: String,
}
