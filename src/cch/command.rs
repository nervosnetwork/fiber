use serde::Deserialize;
use serde_with::serde_as;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum CchCommand {
    SendBTC(SendBTC),
    // TODO(cch): Delete this test RPC, and subscribe to CKB HTLC events to trigger the cross-chain
    // order payment.
    TestPayBTC(TestPayBTC),
    ReceiveBTC(ReceiveBTC),
}

impl CchCommand {
    pub fn name(&self) -> &'static str {
        match self {
            CchCommand::SendBTC(_) => "SendBTC",
            CchCommand::TestPayBTC(_) => "TestPayBTC",
            CchCommand::ReceiveBTC(_) => "ReceiveBTC",
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

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct ReceiveBTC {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,

    /// Identity of the payee CKB lightning node.
    pub payee_pubkey: String,
    /// How many millisatoshis to receive.
    pub amount_msat: u64,
    /// Expiry set for the HTLC for the CKB payment to the payee.
    pub final_tlc_expiry: u64,
}
