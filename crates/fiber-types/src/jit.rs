use serde::{Deserialize, Serialize};

use crate::{Hash256, Pubkey};

/// JIT open and queue links that must survive a process restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedJit {
    pub opens: Vec<PersistedJitOpen>,
    pub queued: Vec<PersistedJitQueue>,
    pub reserved: Vec<PersistedJitReservation>,
}

/// One JIT channel open and the payment it was sized for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedJitOpen {
    pub peer: Pubkey,
    pub channel_id: Hash256,
    pub payment_hash: Hash256,
}

/// Payments waiting for an earlier open to the same receiver to finish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedJitQueue {
    pub peer: Pubkey,
    pub payments: Vec<Hash256>,
}

/// A ready channel whose outbound balance belongs to one payment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedJitReservation {
    pub peer: Pubkey,
    pub channel_id: Hash256,
    pub payment_hash: Hash256,
}
