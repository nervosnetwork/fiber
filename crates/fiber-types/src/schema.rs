//!
//! +--------------+----------------------+-----------------------------+
//! | KeyPrefix::  | Key::                | Value::                     |
//! +--------------+----------------------+-----------------------------+
//! | 0            | Hash256              | ChannelMutableData          |
//! | 1            | Hash256              | ChannelImmutableData        |
//! | 16           | Pubkey               | PersistentNetworkActorState |
//! | 32           | Hash256              | CkbInvoice                  |
//! | 33           | Payment_hash         | CkbInvoice Preimage         |
//! | 34           | Payment_hash         | CkbInvoice Status           |
//! | 35           | Payment_hash         | Invoice-created preimage marker |
//! | 49           | Hash256              | SettlementData               |
//! | 64           | Pubkey | Hash256     | ChannelState                |
//! | 65           | OutPoint             | ChannelId                   |
//! | 66           | Payment_hash | Hash256 | ()                        |
//! | 67           | Hash256              | Vec<Payment_hash>           |
//! | 96           | Cursor               | BroadcastMessage            |
//! | 97           | BroadcastMessageID   | u64                         |
//! | 192          | Hash256              | PaymentSession              |
//! | 193          | OutPoint | Direction | TimedResult                 |
//! | 194          | Hash256              | PaymentCustomRecords        |
//! | 224          | Hash256              | ChannelData                 |
//! | 201          | Hash256              | ChannelOpenRecord           |
//! | 202          | (singleton)          | TenantChannelOpening        |
//! | 232          | Payment_hash         | CchOrder                    |
//! | 233          | Payment_hash         | CchReceiveBtcOrderCreation  |
//! | 234          | Payment_hash         | CchSendBtcOrderCreation    |
//! +--------------+----------------------+-----------------------------+

pub const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
/// Fixed channel fields, written once when the channel opens.
pub const CHANNEL_IMMUTABLE_STATE_PREFIX: u8 = 1;
pub const PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX: u8 = 16;
pub const CKB_INVOICE_PREFIX: u8 = 32;
pub const PREIMAGE_PREFIX: u8 = 33;
pub const CKB_INVOICE_STATUS_PREFIX: u8 = 34;
pub const INVOICE_PREIMAGE_PREFIX: u8 = 35;
pub const CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX: u8 = 49;
pub const PUBKEY_CHANNEL_ID_PREFIX: u8 = 64;
pub const CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX: u8 = 65;
/// Index of channels that record a TLC or a V2 remove effect for a payment hash.
/// Key: [PREFIX, payment_hash, channel_id], Value: ()
pub const PAYMENT_HASH_CHANNEL_ID_PREFIX: u8 = 66;
/// The payment hashes indexed for one channel, to update the index above.
/// Key: [PREFIX, channel_id], Value: Vec<Hash256>, sorted
pub const CHANNEL_PAYMENT_HASHES_PREFIX: u8 = 67;
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
// A shared prefix for watchtower and channel store
pub const WATCHTOWER_TLC_SETTLED_PREFIX: u8 = 200;
pub const CHANNEL_OPEN_RECORD_PREFIX: u8 = 201;
/// Single pending or frozen opening in a tenant's namespaced store.
pub const TENANT_CHANNEL_OPENING_PREFIX: u8 = 202;
/// Original keys and bytes of safely retired V1 records; never loaded by the V2 runtime.
pub const ARCHIVED_V1_PREFIX: u8 = 240;
#[cfg(feature = "watchtower")]
mod watchtower {
    pub const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;
    pub const WATCHTOWER_PREIMAGE_PREFIX: u8 = 225;
    pub const WATCHTOWER_NODE_PAYMENTHASH_PREFIX: u8 = 226;
    pub const WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX: u8 = 227;
    pub const WATCHTOWER_SIGNER_PREFIX: u8 = 228;
}
#[cfg(feature = "watchtower")]
pub use watchtower::*;
#[cfg(not(target_arch = "wasm32"))]
pub const CCH_ORDER_PREFIX: u8 = 232;
#[cfg(not(target_arch = "wasm32"))]
pub const CCH_RECEIVE_BTC_ORDER_CREATION_PREFIX: u8 = 233;
#[cfg(not(target_arch = "wasm32"))]
pub const CCH_SEND_BTC_ORDER_CREATION_PREFIX: u8 = 234;

/// Key of one `PAYMENT_HASH_CHANNEL_ID_PREFIX` row.
pub fn payment_hash_channel_id_key(
    payment_hash: &crate::Hash256,
    channel_id: &crate::Hash256,
) -> Vec<u8> {
    [
        &[PAYMENT_HASH_CHANNEL_ID_PREFIX][..],
        payment_hash.as_ref(),
        channel_id.as_ref(),
    ]
    .concat()
}

/// Key of the `CHANNEL_PAYMENT_HASHES_PREFIX` row of a channel.
pub fn channel_payment_hashes_key(channel_id: &crate::Hash256) -> Vec<u8> {
    [&[CHANNEL_PAYMENT_HASHES_PREFIX][..], channel_id.as_ref()].concat()
}
