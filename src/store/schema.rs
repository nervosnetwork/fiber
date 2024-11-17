///
/// +--------------+--------------------+-----------------------------+
/// | KeyPrefix::  | Key::              | Value::                     |
/// +--------------+--------------------+-----------------------------+
/// | 0            | Hash256            | ChannelActorState           |
/// | 16           | PeerId             | PersistentNetworkActorState |
/// | 32           | Hash256            | CkbInvoice                  |
/// | 33           | Payment_hash       | CkbInvoice Preimage         |
/// | 34           | Payment_hash       | CkbInvoice Status           |
/// | 64           | PeerId | Hash256   | ChannelState                |
/// | 96           | ChannelId          | ChannelInfo                 |
/// | 97           | Block | Index      | ChannelId                   |
/// | 98           | Timestamp          | ChannelId                   |
/// | 128          | NodeId             | NodeInfo                    |
/// | 129          | Timestamp          | NodeId                      |
/// | 160          | PeerId             | MultiAddr                   |
/// | 192          | Hash256            | PaymentSession              |
/// | 193          | NodeId | NodeId    | TimedResult                 |
/// | 224          | Hash256            | ChannelData                 |
/// +--------------+--------------------+-----------------------------+
///

pub(crate) const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
pub(crate) const PEER_ID_NETWORK_ACTOR_STATE_PREFIX: u8 = 16;
pub(crate) const CKB_INVOICE_PREFIX: u8 = 32;
pub(crate) const CKB_INVOICE_PREIMAGE_PREFIX: u8 = 33;
pub(crate) const CKB_INVOICE_STATUS_PREFIX: u8 = 34;
pub(crate) const PEER_ID_CHANNEL_ID_PREFIX: u8 = 64;
pub(crate) const CHANNEL_INFO_PREFIX: u8 = 96;
pub(crate) const CHANNEL_ANNOUNCEMENT_INDEX_PREFIX: u8 = 97;
pub(crate) const CHANNEL_UPDATE_INDEX_PREFIX: u8 = 98;
pub(crate) const NODE_INFO_PREFIX: u8 = 128;
pub(crate) const NODE_ANNOUNCEMENT_INDEX_PREFIX: u8 = 129;
pub(crate) const PAYMENT_SESSION_PREFIX: u8 = 192;
pub(crate) const PAYMENT_HISTORY_TIMED_RESULT_PREFIX: u8 = 193;
pub(crate) const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;
