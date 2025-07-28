//!
//! A full request consists of:
//! [4byte InputCommand] [4byte of payload length (can be zero)] [payload-length bytes of payload (can ib ignored if payload length is zero)]
//!
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use web_sys::js_sys::{Atomics, Int32Array, Uint8Array};
pub enum IteratorMode<'a> {
    Start,
    End,
    From(&'a [u8], DbDirection),
}

impl IteratorMode<'_> {
    pub fn to_owned(&self) -> IteratorModeOwned {
        match self {
            IteratorMode::Start => IteratorModeOwned::Start,
            IteratorMode::End => IteratorModeOwned::End,
            IteratorMode::From(items, db_direction) => {
                IteratorModeOwned::From(items.to_vec(), db_direction.clone())
            }
        }
    }
}

#[derive(Serialize, Debug, Deserialize)]
pub enum IteratorModeOwned {
    Start,
    End,
    From(Vec<u8>, DbDirection),
}
#[derive(Serialize, Debug, Deserialize, Clone)]
pub enum DbDirection {
    Forward,
    Reverse,
}
#[repr(i32)]
pub enum InputCommand {
    /// Indicates that there is no command and light client is waiting for next call
    /// Payload: None
    Waiting = 0,
    /// Open database,
    /// Payload: database name (string), bincode encoded
    OpenDatabase = 1,
    /// Execute a database command
    /// Payload: DbCommandRequest, bincode encoded
    DbRequest = 2,
    /// Shutdown db worker, exiting main loop
    /// Payload: None
    /// Note: There won't be a response for [`crate::InputCommand::Shutdown`]
    Shutdown = 3,
    /// Used for response from take_while, not for users
    /// Payload: result of the call, in bool, bincode-encoded
    PrefixIteratorResponse = 20,
}

impl TryFrom<i32> for InputCommand {
    type Error = anyhow::Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Waiting,
            1 => Self::OpenDatabase,
            2 => Self::DbRequest,
            3 => Self::Shutdown,
            20 => Self::PrefixIteratorResponse,
            s => {
                bail!("Invalid input command: {}", s);
            }
        })
    }
}

#[repr(i32)]
/// Represent a 4-byte command which will be put in output buffer
pub enum OutputCommand {
    /// Waiting for db worker to handle the command
    /// Payload: None
    Waiting = 0,
    /// Successful response of OpenDatabase
    /// Payload: None
    OpenDatabaseResponse = 1,
    /// Successful response of DbRequest
    /// Payload: bincode-encoded DbCommandResponse
    DbResponse = 2,
    /// Error of OpenDatabaseRequest or DbRequest
    /// Payload: bincode-encoded string
    Error = 10,
    /// DbWorker wants to call take_while and yield one entry
    /// Payload: bincode-encoded bytes, argument of take_while
    PrefixIteratorRequestForNextEntry = 20,
}
#[derive(Serialize, Deserialize, Debug)]
/// Represent a key-value pair
pub struct KV {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
impl TryFrom<i32> for OutputCommand {
    type Error = anyhow::Error;

    fn try_from(value: i32) -> Result<Self, <OutputCommand as TryFrom<i32>>::Error> {
        Ok(match value {
            0 => Self::Waiting,
            1 => Self::OpenDatabaseResponse,
            2 => Self::DbResponse,
            10 => Self::Error,
            20 => Self::PrefixIteratorRequestForNextEntry,
            s => {
                bail!("Invalid output command: {}", s);
            }
        })
    }
}
/// Fill a input buffer/output buffer with a [`crate::InputCommand`]/[`crate::OutputCommand`] and the buffer
/// The buffer would be in 4byte command + 4byte payload length + payload
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

/// Read the payload from the given input buffer/output buffer
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

#[derive(Serialize, Deserialize, Debug)]
/// Response of DbCommandRequest. For details, please refer to the doc of DbCommandRequest
pub enum DbCommandResponse {
    Read { values: Vec<Option<Vec<u8>>> },
    Put,
    Delete,
    PrefixIterator { data: Vec<KV> },
}

#[derive(Deserialize, Serialize, Debug)]
/// Represent a database command
pub enum DbCommandRequest {
    /// Read the value corresponding to a series of keys
    /// Input: A series of keys
    /// Output: A series of values corresponding to keys, None if the key wasn't found in database
    Read { keys: Vec<Vec<u8>> },
    /// Write a series of key-value pairs into database
    /// Input: A series of key-value pairs
    /// Output: None
    Put { kvs: Vec<KV> },
    /// Remove a series of entries from database
    /// Input: Keys to remove
    /// Output: None
    Delete { keys: Vec<Vec<u8>> },
    /// Gets at most `limit` entries, starting from `start_key_bound`, skipping the first `skip` entries, keep fetching until `take_while` evals to false
    /// Output: Key value pairs fetched
    PrefixIterator {
        prefix: Vec<u8>,
        mode: IteratorModeOwned,
    },
}
