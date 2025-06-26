//!
//! +--------------+----------------------+-----------------------------+
//! | KeyPrefix::  | Key::                | Value::                     |
//! +--------------+----------------------+-----------------------------+
//! | 0            | Hash256              | ChannelActorState           |
//! | 16           | PeerId               | PersistentNetworkActorState |
//! | 32           | Hash256              | CkbInvoice                  |
//! | 33           | Payment_hash         | CkbInvoice Preimage         |
//! | 34           | Payment_hash         | CkbInvoice Status           |
//! | 64           | PeerId | Hash256     | ChannelState                |
//! | 65           | OutPoint             | ChannelId                   |
//! | 96           | Cursor               | BroadcastMessage            |
//! | 97           | BroadcastMessageID   | u64                         |
//! | 192          | Hash256              | PaymentSession              |
//! | 193          | OutPoint | Direction | TimedResult                 |
//! | 194          | Hash256              | PaymentCustomRecords        |
//! | 224          | Hash256              | ChannelData                 |
//! +--------------+----------------------+-----------------------------+

pub(crate) const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
pub(crate) const PEER_ID_NETWORK_ACTOR_STATE_PREFIX: u8 = 16;
pub(crate) const CKB_INVOICE_PREFIX: u8 = 32;
pub(crate) const PREIMAGE_PREFIX: u8 = 33;
pub(crate) const CKB_INVOICE_STATUS_PREFIX: u8 = 34;
pub(crate) const PEER_ID_CHANNEL_ID_PREFIX: u8 = 64;
pub(crate) const CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX: u8 = 65;
pub(crate) const BROADCAST_MESSAGE_PREFIX: u8 = 96;
pub(crate) const BROADCAST_MESSAGE_TIMESTAMP_PREFIX: u8 = 97;
pub(crate) const PAYMENT_SESSION_PREFIX: u8 = 192;
pub(crate) const PAYMENT_HISTORY_TIMED_RESULT_PREFIX: u8 = 193;
pub(crate) const PAYMENT_CUSTOM_RECORD_PREFIX: u8 = 194;
pub(crate) const ATTEMPT_PREFIX: u8 = 195;
pub(crate) const HOLD_TLC_PREFIX: u8 = 197;
#[cfg(feature = "watchtower")]
pub(crate) const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;
