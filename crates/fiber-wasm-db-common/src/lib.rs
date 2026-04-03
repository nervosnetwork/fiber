//!
//! A full request consists of:
//! [4byte InputCommand] [4byte of payload length (can be zero)] [payload-length bytes of payload (can be ignored if payload length is zero)]
//!
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use web_sys::js_sys::{Atomics, Int32Array, Uint8Array};

// ── Iterator types (matching the StorageBackend trait) ──

/// Direction for iteration (forward or backward).
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorDirection {
    #[default]
    Forward,
    Reverse,
}

// ── IPC commands ──

#[repr(i32)]
pub enum InputCommand {
    /// Indicates that there is no command and light client is waiting for next call.
    /// Payload: None
    Waiting = 0,
    /// Open database.
    /// Payload: database name (string), bincode encoded
    OpenDatabase = 1,
    /// Execute a database command.
    /// Payload: DbCommandRequest, bincode encoded
    DbRequest = 2,
    /// Shutdown db worker, exiting main loop.
    /// Payload: None
    /// Note: There won't be a response for [`InputCommand::Shutdown`]
    Shutdown = 3,
    /// Response from the client for a take_while callback.
    /// Payload: result of the call, in bool, bincode-encoded
    ResponseTakeWhile = 20,
}

impl TryFrom<i32> for InputCommand {
    type Error = anyhow::Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Waiting,
            1 => Self::OpenDatabase,
            2 => Self::DbRequest,
            3 => Self::Shutdown,
            20 => Self::ResponseTakeWhile,
            s => {
                bail!("Invalid input command: {}", s);
            }
        })
    }
}

#[repr(i32)]
#[derive(Debug)]
/// Represent a 4-byte command which will be put in output buffer
pub enum OutputCommand {
    /// Waiting for db worker to handle the command.
    /// Payload: None
    Waiting = 0,
    /// Successful response of OpenDatabase.
    /// Payload: None
    OpenDatabaseResponse = 1,
    /// Successful response of DbRequest.
    /// Payload: bincode-encoded DbCommandResponse
    DbResponse = 2,
    /// Error of OpenDatabaseRequest or DbRequest.
    /// Payload: bincode-encoded string
    Error = 10,
    /// Worker wants the client to evaluate `take_while` for a key.
    /// Payload: bincode-encoded bytes (the key)
    RequestTakeWhile = 20,
}

impl TryFrom<i32> for OutputCommand {
    type Error = anyhow::Error;

    fn try_from(value: i32) -> Result<Self, <OutputCommand as TryFrom<i32>>::Error> {
        Ok(match value {
            0 => Self::Waiting,
            1 => Self::OpenDatabaseResponse,
            2 => Self::DbResponse,
            10 => Self::Error,
            20 => Self::RequestTakeWhile,
            s => {
                bail!("Invalid output command: {}", s);
            }
        })
    }
}

// ── Key-value pair ──

#[derive(Serialize, Deserialize, Debug)]
/// Represent a key-value pair
pub struct KV {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

// ── Database command request/response ──

#[derive(Serialize, Deserialize, Debug)]
/// Response of DbCommandRequest.
pub enum DbCommandResponse {
    Read {
        values: Vec<Option<Vec<u8>>>,
    },
    Put,
    Delete,
    /// Response for Iterator command: the collected key-value pairs.
    Iterator {
        kvs: Vec<KV>,
    },
}

#[derive(Deserialize, Serialize, Debug)]
/// Represent a database command
pub enum DbCommandRequest {
    /// Read the value corresponding to a series of keys.
    /// Input: A series of keys
    /// Output: A series of values corresponding to keys, None if the key wasn't found in database
    Read { keys: Vec<Vec<u8>> },
    /// Write a series of key-value pairs into database.
    /// Input: A series of key-value pairs
    /// Output: None
    Put { kvs: Vec<KV> },
    /// Remove a series of entries from database.
    /// Input: Keys to remove
    /// Output: None
    Delete { keys: Vec<Vec<u8>> },
    /// Iterate over key-value pairs, with `take_while` evaluated via IPC callback.
    /// The worker iterates from `start` in `direction`, calling back to the client
    /// for each key to evaluate `take_while`. Stops when `take_while` returns false
    /// or `limit` entries have been collected (0 = no limit).
    /// Output: Key value pairs fetched
    Iterator {
        start: Vec<u8>,
        direction: IteratorDirection,
        limit: usize,
    },
}

// ── Serialization helpers ──

/// Fill a input buffer/output buffer with a [`InputCommand`]/[`OutputCommand`] and the buffer.
/// The buffer would be in 4byte command + 4byte payload length + payload.
///
/// cmd: Command in i32
/// data: The payload of command
/// i32arr: Int32Array view of the buffer
/// u8arr: Uint8Array view of the buffer
pub fn write_command_with_payload<T: Serialize>(
    cmd: i32,
    data: T,
    i32arr: &Int32Array,
    u8arr: &Uint8Array,
) -> anyhow::Result<()> {
    let result_buf = bincode::serde::encode_to_vec(&data, bincode::config::standard())
        .with_context(|| anyhow!("Failed to serialize command payload"))?;

    i32arr.set_index(1, result_buf.len() as i32);
    u8arr
        .subarray(8, 8 + result_buf.len() as u32)
        .copy_from(&result_buf);
    i32arr.set_index(0, cmd);
    Atomics::notify(i32arr, 0).map_err(|e| anyhow!("Failed to notify: {e:?}"))?;
    Ok(())
}

/// Read the payload from the given input buffer/output buffer.
/// i32arr: Int32Array view of the buffer
/// u8arr: Uint8Array view of the buffer
pub fn read_command_payload<T: DeserializeOwned>(
    i32arr: &Int32Array,
    u8arr: &Uint8Array,
) -> anyhow::Result<T> {
    let length = i32arr.get_index(1) as u32;
    let mut buf = vec![0u8; length as usize];
    u8arr.subarray(8, 8 + length).copy_to(&mut buf);

    let (result, _) = bincode::serde::decode_from_slice::<T, _>(&buf, bincode::config::standard())
        .with_context(|| anyhow!("Failed to decode command"))?;
    Ok(result)
}
