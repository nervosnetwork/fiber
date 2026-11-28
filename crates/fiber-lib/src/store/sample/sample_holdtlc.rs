/// StoreSample implementation for `HoldTlc`.
use crate::fiber::types::HoldTlc;
use fiber_types::schema::HOLD_TLC_PREFIX;

use super::{deterministic_hash256, StoreSample};

impl StoreSample for HoldTlc {
    const STORE_PREFIX: u8 = HOLD_TLC_PREFIX;
    const TYPE_NAME: &'static str = "HoldTlc";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            HoldTlc {
                channel_id: deterministic_hash256(seed, 0),
                tlc_id: 0,
                hold_expire_at: 1_704_067_200_000,
            },
            HoldTlc {
                channel_id: deterministic_hash256(seed, 1),
                tlc_id: 42,
                hold_expire_at: 1_704_070_800_000,
            },
        ]
    }
}

#[cfg(test)]
mod tests;
