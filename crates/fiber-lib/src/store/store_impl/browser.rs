use super::check_migrate;
use super::KeyValue;
use super::StoreKeyValue;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use fiber_wasm_db_common::read_command_payload;
use fiber_wasm_db_common::write_command_with_payload;
use fiber_wasm_db_common::DbCommandRequest;
use fiber_wasm_db_common::DbCommandResponse;
pub use fiber_wasm_db_common::DbDirection;
use fiber_wasm_db_common::InputCommand;
pub use fiber_wasm_db_common::IteratorMode;
use fiber_wasm_db_common::IteratorModeOwned;
use fiber_wasm_db_common::OutputCommand;
use fiber_wasm_db_common::KV;
use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use tracing::debug;
use tracing::info;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::js_sys::Atomics;
use web_sys::js_sys::Int32Array;
use web_sys::js_sys::SharedArrayBuffer;
use web_sys::js_sys::Uint8Array;

unsafe impl Send for Store {}
unsafe impl Sync for Store {}

#[derive(Clone)]
pub struct Store {
    chan: CommunicationChannel,
}
impl Store {
    /// Open a store, with migration check
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let store = Self::open_db(path.as_ref())?;
        let store = check_migrate(path, store)?;
        Ok(store)
    }
    /// Open a store, without migration check
    pub fn open_db(path: &Path) -> Result<Self, String> {
        let chan = CommunicationChannel::prepare_from_global();
        if !DB_INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
            chan.open_database(path.to_str().unwrap());
            DB_INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            debug!("Database has already been initialized");
        }
        Ok(Self { chan })
    }
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        return match self
            .chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Read {
                keys: vec![key.as_ref().to_vec()],
            })
            .unwrap()
        {
            DbCommandResponse::Read { mut values } => values.remove(0),
            _ => unreachable!(),
        };
    }
    pub fn delete<K: AsRef<[u8]>>(&self, key: K) {
        match self
            .chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Delete {
                keys: vec![key.as_ref().to_vec()],
            })
            .unwrap()
        {
            DbCommandResponse::Delete {} => {}
            _ => unreachable!(),
        };
    }
    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        match self
            .chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Put {
                kvs: vec![KV {
                    key: key.as_ref().to_vec(),
                    value: value.as_ref().to_vec(),
                }],
            })
            .unwrap()
        {
            DbCommandResponse::Put {} => {}
            _ => unreachable!(),
        };
    }
    pub fn batch(&self) -> Batch {
        Batch {
            chan: self.chan.clone(),
            delete: vec![],
            puts: vec![],
        }
    }
    #[allow(clippy::type_complexity)]
    pub fn prefix_iterator_with_skip_while_and_start<'a>(
        &'a self,
        prefix: &'a [u8],
        mode: IteratorMode<'a>,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        match self
            .chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::PrefixIterator {
                prefix: prefix.to_vec(),
                mode: mode.to_owned(),
                skip_while,
            })
            .unwrap()
        {
            DbCommandResponse::PrefixIterator { data } => data
                .into_iter()
                .map(|kv| (kv.key.into_boxed_slice(), kv.value.into_boxed_slice())),
            _ => unreachable!(),
        }
    }
    pub fn prefix_iterator<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.prefix_iterator_with_skip_while_and_start(
            prefix,
            IteratorMode::From(prefix, DbDirection::Forward),
            Box::new(|_| false),
        )
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
pub struct Batch {
    chan: CommunicationChannel,
    puts: Vec<KV>,
    delete: Vec<Vec<u8>>,
}
impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        return match self
            .chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Read {
                keys: vec![key.as_ref().to_vec()],
            })
            .unwrap()
        {
            DbCommandResponse::Read { mut values } => values.remove(0),
            _ => unreachable!(),
        };
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        self.puts.push(KV {
            key: key_value.key(),
            value: key_value.value(),
        });
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.puts.push(KV {
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.delete.push(key.as_ref().to_vec());
    }

    pub fn commit(self) {
        self.chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Delete {
                keys: self.delete,
            })
            .expect("Failed to delete batch");
        self.chan
            .dispatch_database_command(DbCommandRequestWithSkipWhileFunc::Put { kvs: self.puts })
            .expect("Failed to put batch");
    }
}

thread_local! {
    static INPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
    static OUTPUT_BUFFER: RefCell<Option<SharedArrayBuffer>> = const { RefCell::new(None) };
}
static DB_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[wasm_bindgen]
/// Set `SharedArrayBuffer` used for communicating with light client worker. This must be called before executing `main_loop`
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
    /// Create a [`crate::storage::db::browser::CommunicationChannel`] from global stored buffers
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
            OutputCommand::PrefixIteratorRequestForNextEntry
            | OutputCommand::Waiting
            | OutputCommand::DbResponse => {
                unreachable!()
            }
        }
    }

    /// Executa a database command, retriving the response (or error)
    /// cmd: The command
    fn dispatch_database_command(
        &self,
        cmd: DbCommandRequestWithSkipWhileFunc,
    ) -> anyhow::Result<DbCommandResponse> {
        let (new_cmd, skip_while) = match cmd {
            DbCommandRequestWithSkipWhileFunc::Read { keys } => {
                (DbCommandRequest::Read { keys }, None)
            }
            DbCommandRequestWithSkipWhileFunc::Put { kvs } => (DbCommandRequest::Put { kvs }, None),
            DbCommandRequestWithSkipWhileFunc::Delete { keys } => {
                (DbCommandRequest::Delete { keys }, None)
            }
            DbCommandRequestWithSkipWhileFunc::PrefixIterator {
                prefix,
                mode,
                skip_while,
            } => (
                DbCommandRequest::PrefixIterator { prefix, mode },
                Some(skip_while),
            ),
        };
        debug!("Dispatching database command: {:?}", new_cmd);
        let CommunicationChannel {
            input_i32_arr,
            input_u8_arr,
            output_i32_arr,
            output_u8_arr,
        } = self;
        output_i32_arr.set_index(0, InputCommand::Waiting as i32);
        write_command_with_payload(
            InputCommand::DbRequest as i32,
            new_cmd,
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
                OutputCommand::PrefixIteratorRequestForNextEntry => {
                    let arg = read_command_payload::<Vec<u8>>(output_i32_arr, output_u8_arr)?;
                    let ok = skip_while.as_ref().unwrap()(&arg);

                    debug!(
                        "Received take while request with args {:?}, result {}",
                        arg, ok
                    );
                    write_command_with_payload(
                        InputCommand::PrefixIteratorResponse as i32,
                        ok,
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

pub enum DbCommandRequestWithSkipWhileFunc {
    Read {
        keys: Vec<Vec<u8>>,
    },
    Put {
        kvs: Vec<KV>,
    },
    Delete {
        keys: Vec<Vec<u8>>,
    },
    PrefixIterator {
        prefix: Vec<u8>,
        mode: IteratorModeOwned,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    },
}
