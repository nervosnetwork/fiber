use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use fiber_wasm_db_common::read_command_payload;
use fiber_wasm_db_common::write_command_with_payload;
use fiber_wasm_db_common::DbCommandRequest;
use fiber_wasm_db_common::DbCommandResponse;
use fiber_wasm_db_common::InputCommand;
use fiber_wasm_db_common::OutputCommand;
use fiber_wasm_db_common::KV;
use std::cell::RefCell;
use std::fmt::Debug;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use tracing::info;
use tracing::trace;
use tracing::warn;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::js_sys::Atomics;
use web_sys::js_sys::Int32Array;
use web_sys::js_sys::SharedArrayBuffer;
use web_sys::js_sys::Uint8Array;

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::iterator::{IteratorDirection, KVPair};

type TakeWhileCallback = Box<dyn Fn(&[u8]) -> bool + 'static>;

unsafe impl Send for Store {}
unsafe impl Sync for Store {}

#[derive(Clone)]
pub struct Store {
    chan: CommunicationChannel,
}

impl Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "BrowserStore")?;
        Ok(())
    }
}

impl Store {
    /// Open a store (without migration check — use `check_migrate` or `DbMigrate` for that)
    pub fn open_db(path: &Path) -> Result<Self, String> {
        let chan = CommunicationChannel::prepare_from_global();
        if !DB_INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
            let path_str = path
                .to_str()
                .ok_or_else(|| "database path is not valid UTF-8".to_string())?;
            chan.open_database(path_str);
            DB_INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            warn!("Database has already been initialized");
        }
        Ok(Self { chan })
    }

    pub fn shutdown(self) {
        info!("Shutting down database Store..");
        let CommunicationChannel {
            input_i32_arr,
            input_u8_arr,
            output_i32_arr,
            ..
        } = &self.chan;
        output_i32_arr.set_index(0, InputCommand::Waiting as i32);
        write_command_with_payload(
            InputCommand::Shutdown as i32,
            (),
            input_i32_arr,
            input_u8_arr,
        )
        .unwrap();
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        return match self
            .chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Read {
                keys: vec![key.as_ref().to_vec()],
            })
            .unwrap()
        {
            DbCommandResponse::Read { mut values } => values.remove(0),
            _ => unreachable!(),
        };
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        match self
            .chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Put {
                kvs: vec![KV {
                    key: key.as_ref().to_vec(),
                    value: value.as_ref().to_vec(),
                }],
            })
            .unwrap()
        {
            DbCommandResponse::Put => {}
            _ => unreachable!(),
        };
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        match self
            .chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Delete {
                keys: vec![key.as_ref().to_vec()],
            })
            .unwrap()
        {
            DbCommandResponse::Delete => {}
            _ => unreachable!(),
        };
    }

    fn batch(&self) -> Self::Batch {
        Batch {
            chan: self.chan.clone(),
            delete: vec![],
            puts: vec![],
        }
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        let ipc_direction = match direction {
            IteratorDirection::Forward => fiber_wasm_db_common::IteratorDirection::Forward,
            IteratorDirection::Reverse => fiber_wasm_db_common::IteratorDirection::Reverse,
        };

        // Wrap the take_while_fn into a boxed callback for IPC
        let take_while: TakeWhileCallback = Box::new(move |key: &[u8]| take_while_fn(key));

        let kvs = match self
            .chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Iterator {
                start,
                direction: ipc_direction,
                limit,
                take_while,
            })
            .unwrap()
        {
            DbCommandResponse::Iterator { kvs } => kvs,
            _ => unreachable!(),
        };

        kvs.into_iter()
            .map(|kv| KVPair {
                key: kv.key,
                value: kv.value,
            })
            .collect()
    }
}

pub struct Batch {
    chan: CommunicationChannel,
    puts: Vec<KV>,
    delete: Vec<Vec<u8>>,
}

impl BatchWriter for Batch {
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.puts.push(KV {
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.delete.push(key.as_ref().to_vec());
    }

    fn commit(self) {
        self.chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Delete { keys: self.delete })
            .expect("Failed to delete batch");
        self.chan
            .dispatch_database_command(DbCommandRequestWithTakeWhile::Put { kvs: self.puts })
            .expect("Failed to put batch");
    }
}

thread_local! {
    static INPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
    static OUTPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
}
static DB_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[wasm_bindgen]
/// Set `SharedArrayBuffer` used for communicating with light client worker. This must be called
/// before executing `main_loop`.
/// input - The buffer used for sending data from light client worker to db worker
/// output - The buffer used for sending data from db worker to light client worker
pub fn set_shared_array(input: JsValue, output: JsValue) {
    console_error_panic_hook::set_once();
    INPUT_BUFFER.with(|v| {
        *v.borrow_mut() = Some(input.dyn_into().unwrap());
    });
    OUTPUT_BUFFER.with(|v| {
        *v.borrow_mut() = Some(output.dyn_into().unwrap());
    });
}

#[derive(Clone)]
/// The channel used for communicating with db worker
struct CommunicationChannel {
    input_i32_arr: Int32Array,
    input_u8_arr: Uint8Array,
    output_i32_arr: Int32Array,
    output_u8_arr: Uint8Array,
}

impl CommunicationChannel {
    /// Create a [`CommunicationChannel`] from global stored buffers
    fn prepare_from_global() -> Self {
        let (input_i32_arr, input_u8_arr) = INPUT_BUFFER.with(|x| {
            let binding = x.borrow();
            let buf = binding.as_ref().unwrap();
            (Int32Array::new(buf), Uint8Array::new(buf))
        });
        let (output_i32_arr, output_u8_arr) = OUTPUT_BUFFER.with(|x| {
            let binding = x.borrow();
            let buf = binding.as_ref().unwrap();
            (Int32Array::new(buf), Uint8Array::new(buf))
        });
        Self {
            input_i32_arr,
            input_u8_arr,
            output_i32_arr,
            output_u8_arr,
        }
    }

    /// Open the database
    fn open_database(&self, store_name: &str) {
        let CommunicationChannel {
            input_i32_arr,
            input_u8_arr,
            output_i32_arr,
            output_u8_arr,
        } = &self;
        output_i32_arr.set_index(0, InputCommand::Waiting as i32);
        write_command_with_payload(
            InputCommand::OpenDatabase as i32,
            store_name,
            input_i32_arr,
            input_u8_arr,
        )
        .with_context(|| anyhow!("Failed to write db command"))
        .unwrap();
        Atomics::wait(output_i32_arr, 0, OutputCommand::Waiting as i32).unwrap();
        let output_cmd = OutputCommand::try_from(output_i32_arr.get_index(0)).unwrap();
        match output_cmd {
            OutputCommand::OpenDatabaseResponse => {
                DB_INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            OutputCommand::Error => panic!(
                "{}",
                read_command_payload::<String>(output_i32_arr, output_u8_arr).unwrap()
            ),
            OutputCommand::RequestTakeWhile
            | OutputCommand::Waiting
            | OutputCommand::DbResponse => {
                unreachable!()
            }
        }
    }

    /// Execute a database command, retrieving the response (or error).
    ///
    /// For `Iterator` commands, this handles the `RequestTakeWhile` IPC callback
    /// loop: the worker sends each key for evaluation, and this method responds
    /// with the result of `take_while`.
    fn dispatch_database_command(
        &self,
        cmd: DbCommandRequestWithTakeWhile,
    ) -> anyhow::Result<DbCommandResponse> {
        let (ipc_cmd, take_while) = match cmd {
            DbCommandRequestWithTakeWhile::Read { keys } => (DbCommandRequest::Read { keys }, None),
            DbCommandRequestWithTakeWhile::Put { kvs } => (DbCommandRequest::Put { kvs }, None),
            DbCommandRequestWithTakeWhile::Delete { keys } => {
                (DbCommandRequest::Delete { keys }, None)
            }
            DbCommandRequestWithTakeWhile::Iterator {
                start,
                direction,
                limit,
                take_while,
            } => (
                DbCommandRequest::Iterator {
                    start,
                    direction,
                    limit,
                },
                Some(take_while),
            ),
        };
        trace!("Dispatching database command: {:?}", ipc_cmd);
        let CommunicationChannel {
            input_i32_arr,
            input_u8_arr,
            output_i32_arr,
            output_u8_arr,
        } = self;
        output_i32_arr.set_index(0, InputCommand::Waiting as i32);
        write_command_with_payload(
            InputCommand::DbRequest as i32,
            ipc_cmd,
            input_i32_arr,
            input_u8_arr,
        )
        .with_context(|| anyhow!("Failed to write db command"))?;
        loop {
            Atomics::wait(output_i32_arr, 0, OutputCommand::Waiting as i32).unwrap();
            let output_cmd = OutputCommand::try_from(output_i32_arr.get_index(0)).unwrap();
            output_i32_arr.set_index(0, 0);
            match output_cmd {
                OutputCommand::OpenDatabaseResponse | OutputCommand::Waiting => unreachable!(),
                OutputCommand::RequestTakeWhile => {
                    // Worker is asking us to evaluate take_while for a key
                    let key = read_command_payload::<Vec<u8>>(output_i32_arr, output_u8_arr)?;
                    let result = take_while.as_ref().expect(
                        "Received RequestTakeWhile but no take_while callback was provided",
                    )(&key);

                    trace!(
                        "Received take_while request for key {:?}, result {}",
                        key,
                        result
                    );
                    write_command_with_payload(
                        InputCommand::ResponseTakeWhile as i32,
                        result,
                        input_i32_arr,
                        input_u8_arr,
                    )?;
                    continue;
                }
                OutputCommand::DbResponse => {
                    return read_command_payload::<DbCommandResponse>(
                        output_i32_arr,
                        output_u8_arr,
                    );
                }
                OutputCommand::Error => {
                    let payload = read_command_payload::<String>(output_i32_arr, output_u8_arr)?;
                    bail!("{}", payload);
                }
            }
        }
    }
}

/// Client-side wrapper around `DbCommandRequest` that carries a non-serializable
/// `take_while` callback for `Iterator` commands. The callback is extracted by
/// `dispatch_database_command` and used to respond to `RequestTakeWhile` IPC
/// messages from the worker.
pub enum DbCommandRequestWithTakeWhile {
    Read {
        keys: Vec<Vec<u8>>,
    },
    Put {
        kvs: Vec<KV>,
    },
    Delete {
        keys: Vec<Vec<u8>>,
    },
    Iterator {
        start: Vec<u8>,
        direction: fiber_wasm_db_common::IteratorDirection,
        limit: usize,
        #[allow(clippy::type_complexity)]
        take_while: TakeWhileCallback,
    },
}
