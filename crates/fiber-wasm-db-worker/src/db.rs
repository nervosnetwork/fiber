use crate::KV;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use fiber_wasm_db_common::DbCommandRequest;
use fiber_wasm_db_common::DbCommandResponse;
use fiber_wasm_db_common::IteratorModeOwned;
use idb::CursorDirection;
use idb::Database;
use idb::DatabaseEvent;
use idb::Factory;
use idb::IndexParams;
use idb::KeyPath;
use idb::KeyRange;
use idb::ObjectStore;
use idb::ObjectStoreParams;
use idb::TransactionMode;
use idb::TransactionResult;
use log::debug;

pub(crate) async fn handle_prefix_iterator<F>(
    store: &ObjectStore,
    prefix: &[u8],
    mode: IteratorModeOwned,
    skip_while: F,
) -> anyhow::Result<Vec<KV>>
where
    F: Fn(&[u8]) -> bool,
{
    let cursor = match mode {
        IteratorModeOwned::Start => store.open_cursor(None, Some(idb::CursorDirection::Next)),
        IteratorModeOwned::End => store.open_cursor(None, Some(idb::CursorDirection::Prev)),
        IteratorModeOwned::From(items, db_direction) => match db_direction {
            fiber_wasm_db_common::DbDirection::Forward => store.open_cursor(
                Some(idb::Query::KeyRange(
                    KeyRange::lower_bound(
                        &serde_wasm_bindgen::to_value(&items).unwrap(),
                        Some(false),
                    )
                    .expect("Unable to create keyrange"),
                )),
                Some(CursorDirection::Next),
            ),
            fiber_wasm_db_common::DbDirection::Reverse => store.open_cursor(
                Some(idb::Query::KeyRange(
                    KeyRange::upper_bound(
                        &serde_wasm_bindgen::to_value(&items).unwrap(),
                        Some(false),
                    )
                    .expect("Unable to create keyrange"),
                )),
                Some(CursorDirection::Prev),
            ),
        },
    }
    .map_err(|e| anyhow!("Unable to create cursor: {}", e))?
    .await
    .map_err(|e| anyhow!("Unable to perform cursor requests: {}", e))?;
    let mut cursor = match cursor {
        Some(cursor) => cursor.into_managed(),
        None => {
            debug!("No records found, returning");
            return Ok(vec![]);
        }
    };

    let mut result = vec![];
    loop {
        let key: Vec<u8> = serde_wasm_bindgen::from_value(
            cursor
                .key()
                .map_err(|e| anyhow!("Unable to read key from cursor: {}", e))?
                .expect("Expect non-null key"),
        )
        .unwrap();
        let value: Vec<u8> = serde_wasm_bindgen::from_value(
            cursor
                .value()
                .map_err(|e| anyhow!("Unable to read value from cursor: {}", e))?
                .expect("Expect non-null value"),
        )
        .unwrap();
        if skip_while(&key) {
            debug!("Skip while returns true, skipping");
        } else if key.starts_with(prefix) {
            result.push(KV { key, value });
        } else {
            debug!("Prefix ended, breaking..");
            break;
        }
        match cursor.next(None).await {
            Ok(_) => {
                debug!("Cursor next..");
                continue;
            }
            Err(idb::Error::CursorFinished) => {
                debug!("Cursor finished, exiting..");
                break;
            }
            Err(e) => {
                bail!("Unexpected error when iterating: {}", e);
            }
        }
    }
    Ok(result)
}
pub(crate) async fn handle_db_command<F>(
    db: &Database,
    store_name: &str,
    cmd: DbCommandRequest,
    invoke_skip_while: F,
) -> anyhow::Result<DbCommandResponse>
where
    F: Fn(&[u8]) -> bool,
{
    debug!("Handle command: {:?}", cmd);
    let tx_mode = match cmd {
        DbCommandRequest::Read { .. } | DbCommandRequest::PrefixIterator { .. } => {
            TransactionMode::ReadOnly
        }
        DbCommandRequest::Put { .. } | DbCommandRequest::Delete { .. } => {
            TransactionMode::ReadWrite
        }
    };
    let tran = db
        .transaction(&[&store_name], tx_mode)
        .map_err(|e| anyhow!("Failed to create transaction: {:?}", e))?;

    let store = tran
        .object_store(store_name)
        .map_err(|e| anyhow!("Unable to find store {}: {}", store_name, e))?;

    let result = match cmd {
        DbCommandRequest::Read { keys } => {
            let mut res = Vec::new();
            for key in keys {
                let key = serde_wasm_bindgen::to_value(&key)
                    .map_err(|e| anyhow!("Unable to convert key to JsValue: {:?}", e))?;

                res.push(
                    store
                        .get(key)
                        .map_err(|e| anyhow!("Failed to send get request: {:?}", e))?
                        .await
                        .map_err(|e| anyhow!("Failed to fetch value: {:?}", e))?
                        .map(|v| serde_wasm_bindgen::from_value::<Vec<u8>>(v).unwrap()),
                );
            }
            DbCommandResponse::Read { values: res }
        }
        DbCommandRequest::Put { kvs } => {
            debug!("Putting: {:?}", kvs);
            for KV { key, value } in kvs {
                let key = serde_wasm_bindgen::to_value(&key).unwrap();
                let value = serde_wasm_bindgen::to_value(&value).unwrap();
                store
                    .put(&value, Some(&key))
                    .map_err(|e| anyhow!("Failed to send put request: {:?}", e))?
                    .await
                    .map_err(|e| anyhow!("Failed to put: {:?}", e))?;
            }
            DbCommandResponse::Put
        }
        DbCommandRequest::Delete { keys } => {
            for key in keys {
                let key = serde_wasm_bindgen::to_value(&key).unwrap();
                store
                    .delete(key)
                    .map_err(|e| anyhow!("Failed to send delete request: {:?}", e))?
                    .await
                    .map_err(|e| anyhow!("Failed to delete: {:?}", e))?;
            }
            DbCommandResponse::Delete
        }
        DbCommandRequest::PrefixIterator { prefix, mode } => {
            let result = handle_prefix_iterator(&store, &prefix, mode, invoke_skip_while)
                .await
                .with_context(|| anyhow!("Unable to handle prefix iterator"))?;

            DbCommandResponse::PrefixIterator { data: result }
        }
    };
    assert_eq!(TransactionResult::Committed, tran.await.unwrap());
    debug!("Command result={:?}", result);
    Ok(result)
}

pub const STORE_NAME: &str = "main-store";

pub(crate) async fn open_database(database_name: impl AsRef<str>) -> anyhow::Result<Database> {
    let factory = Factory::new().map_err(|e| anyhow!("Failed to create db factory: {:?}", e))?;
    let mut open_request = factory
        .open(database_name.as_ref(), Some(1))
        .map_err(|e| anyhow!("Failed to send db open request: {:?}", e))?;
    open_request.on_upgrade_needed(move |event| {
        let database = event.database().unwrap();
        let store_params = ObjectStoreParams::new();

        let store = database
            .create_object_store(STORE_NAME, store_params)
            .unwrap();
        let mut index_params = IndexParams::new();
        index_params.unique(true);
        store
            .create_index("key", KeyPath::new_single("key"), Some(index_params))
            .unwrap();
    });
    open_request
        .await
        .map_err(|e| anyhow!("Failed to open database: {:?}", e))
}
