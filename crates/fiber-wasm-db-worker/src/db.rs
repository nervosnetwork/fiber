use crate::KV;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use fiber_wasm_db_common::DbCommandRequest;
use fiber_wasm_db_common::DbCommandResponse;
use fiber_wasm_db_common::IteratorDirection;
use idb::CursorDirection;
use idb::Database;
use idb::DatabaseEvent;
use idb::Factory;
use idb::IndexParams;
use idb::KeyPath;
use idb::KeyRange;
use idb::ManagedCursor;
use idb::ObjectStore;
use idb::ObjectStoreParams;
use idb::TransactionMode;
use idb::TransactionResult;
use log::debug;

/// Open an IDB cursor starting from `start_key_bound` in the given `direction`.
async fn open_cursor(
    store: &ObjectStore,
    start_key_bound: &[u8],
    direction: IteratorDirection,
) -> Result<ManagedCursor, idb::Error> {
    let cursor_direction = match direction {
        IteratorDirection::Forward => CursorDirection::Next,
        IteratorDirection::Reverse => CursorDirection::Prev,
    };

    let key_range = match direction {
        IteratorDirection::Forward => KeyRange::lower_bound(
            &serde_wasm_bindgen::to_value(&start_key_bound).unwrap(),
            Some(false), // inclusive
        ),
        IteratorDirection::Reverse => KeyRange::upper_bound(
            &serde_wasm_bindgen::to_value(&start_key_bound).unwrap(),
            Some(false), // inclusive
        ),
    }
    .expect("Unable to create KeyRange");

    store
        .open_cursor(
            Some(idb::Query::KeyRange(key_range)),
            Some(cursor_direction),
        )?
        .await?
        .map(|c| c.into_managed())
        .ok_or(idb::Error::CursorFinished)
}

/// Collect key-value pairs from the store, calling `take_while` for each key
/// via the provided callback. Iteration stops when `take_while` returns `false`
/// or `limit` entries have been collected.
pub(crate) async fn collect_iterator<F>(
    store: &ObjectStore,
    start: &[u8],
    direction: IteratorDirection,
    take_while: F,
    limit: usize,
) -> anyhow::Result<Vec<KV>>
where
    F: Fn(&[u8]) -> bool,
{
    let mut cursor = match open_cursor(store, start, direction).await {
        Ok(c) => c,
        Err(idb::Error::CursorFinished) => return Ok(vec![]),
        Err(e) => return Err(anyhow!("Failed to open cursor: {e:?}")),
    };

    let mut results = Vec::new();

    loop {
        // Read current key
        let key: Vec<u8> = match cursor
            .key()
            .map_err(|e| anyhow!("Unable to read key from cursor: {e:?}"))?
        {
            Some(v) => serde_wasm_bindgen::from_value(v).unwrap(),
            None => break,
        };

        // Read current value
        let value: Vec<u8> = serde_wasm_bindgen::from_value(
            cursor
                .value()
                .map_err(|e| anyhow!("Unable to read value from cursor: {e:?}"))?
                .expect("Value must exist for cursor entry"),
        )
        .unwrap();

        // Check take_while via IPC callback
        if !take_while(&key) {
            debug!("take_while returned false, stopping iteration");
            break;
        }

        results.push(KV { key, value });

        // Check limit
        if limit > 0 && results.len() >= limit {
            break;
        }

        // Advance cursor
        match cursor.next(None).await {
            Ok(_) => continue,
            Err(idb::Error::CursorFinished) => break,
            Err(e) => bail!("Unexpected error when iterating: {e}"),
        }
    }

    Ok(results)
}

pub(crate) async fn handle_db_command<F>(
    db: &Database,
    store_name: &str,
    cmd: DbCommandRequest,
    invoke_take_while: F,
) -> anyhow::Result<DbCommandResponse>
where
    F: Fn(&[u8]) -> bool,
{
    debug!("Handle command: {:?}", cmd);
    let tx_mode = match cmd {
        DbCommandRequest::Read { .. } | DbCommandRequest::Iterator { .. } => {
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
        DbCommandRequest::Iterator {
            start,
            direction,
            limit,
        } => {
            let kvs = collect_iterator(&store, &start, direction, invoke_take_while, limit)
                .await
                .with_context(|| anyhow!("Unable to handle iterator"))?;
            debug!(
                "Called iterator, args=<start={} bytes, {:?}, {:?}>, result_count={}",
                start.len(),
                direction,
                limit,
                kvs.len()
            );
            DbCommandResponse::Iterator { kvs }
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
