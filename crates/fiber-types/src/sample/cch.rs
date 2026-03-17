//! CCH order sample data (non-WASM only).
#![cfg(not(target_arch = "wasm32"))]

use crate::cch::CchOrderStatus;
use crate::schema::CCH_ORDER_PREFIX;

use super::StoreSample;

impl StoreSample for CchOrderStatus {
    const STORE_PREFIX: u8 = CCH_ORDER_PREFIX; // Reuse prefix, this is just for testing
    const TYPE_NAME: &'static str = "CchOrderStatus";

    fn samples(_seed: u64) -> Vec<Self> {
        vec![
            CchOrderStatus::Pending,
            CchOrderStatus::IncomingAccepted,
            CchOrderStatus::OutgoingInFlight,
            CchOrderStatus::OutgoingSuccess,
            CchOrderStatus::Success,
            CchOrderStatus::Failed,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cch_order_status_samples_roundtrip() {
        CchOrderStatus::verify_samples_roundtrip(42);
    }
}
