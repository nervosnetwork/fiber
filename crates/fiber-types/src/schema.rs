//!
//! +--------------+----------------------+-----------------------------+
//! | KeyPrefix::  | Key::                | Value::                     |
//! +--------------+----------------------+-----------------------------+
//! | 0            | Hash256              | ChannelActorState           |
//! | 16           | Pubkey               | PersistentNetworkActorState |
//! | 32           | Hash256              | CkbInvoice                  |
//! | 33           | Payment_hash         | CkbInvoice Preimage         |
//! | 34           | Payment_hash         | CkbInvoice Status           |
//! | 48           | Hash256              | PendingCommitDiff           |
//! | 64           | Pubkey | Hash256     | ChannelState                |
//! | 65           | OutPoint             | ChannelId                   |
//! | 96           | Cursor               | BroadcastMessage            |
//! | 97           | BroadcastMessageID   | u64                         |
//! | 192          | Hash256              | PaymentSession              |
//! | 193          | OutPoint | Direction | TimedResult                 |
//! | 194          | Hash256              | PaymentCustomRecords        |
//! | 224          | Hash256              | ChannelData                 |
//! | 201          | Hash256              | ChannelOpenRecord           |
//! | 232          | Payment_hash         | CchOrder                    |
//! +--------------+----------------------+-----------------------------+

pub const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
pub const PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX: u8 = 16;
pub const CKB_INVOICE_PREFIX: u8 = 32;
pub const PREIMAGE_PREFIX: u8 = 33;
pub const CKB_INVOICE_STATUS_PREFIX: u8 = 34;
pub const PENDING_COMMIT_DIFF_PREFIX: u8 = 48;
pub const PUBKEY_CHANNEL_ID_PREFIX: u8 = 64;
pub const CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX: u8 = 65;
pub const BROADCAST_MESSAGE_PREFIX: u8 = 96;
pub const BROADCAST_MESSAGE_TIMESTAMP_PREFIX: u8 = 97;
pub const PAYMENT_SESSION_PREFIX: u8 = 192;
pub const PAYMENT_HISTORY_TIMED_RESULT_PREFIX: u8 = 193;
pub const PAYMENT_CUSTOM_RECORD_PREFIX: u8 = 194;
pub const ATTEMPT_PREFIX: u8 = 195;
// Index for attempts by first hop channel outpoint
// Key: [PREFIX, channel_outpoint, payment_hash, attempt_id], Value: ()
pub const ATTEMPT_CHANNEL_INDEX_PREFIX: u8 = 196;
pub const HOLD_TLC_PREFIX: u8 = 197;
#[cfg(feature = "watchtower")]
pub const WATCHTOWER_TLC_SETTLED_PREFIX: u8 = 200;
pub const CHANNEL_OPEN_RECORD_PREFIX: u8 = 201;
#[cfg(feature = "watchtower")]
mod watchtower {
    pub const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;
    pub const WATCHTOWER_PREIMAGE_PREFIX: u8 = 225;
    pub const WATCHTOWER_NODE_PAYMENTHASH_PREFIX: u8 = 226;
}
#[cfg(feature = "watchtower")]
pub use watchtower::*;
#[cfg(not(target_arch = "wasm32"))]
pub const CCH_ORDER_PREFIX: u8 = 232;
