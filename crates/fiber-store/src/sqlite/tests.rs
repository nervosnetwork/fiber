use super::*;

#[test]
fn multi_get_returns_values_in_the_requested_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_db(dir.path()).unwrap();
    let mut batch = store.batch();
    batch.put(b"b", b"second");
    batch.put(b"a", b"first");
    batch.commit();

    // Missing keys stay in place, and the result follows the request order rather than the
    // order SQLite happens to return.
    assert_eq!(
        store.multi_get(&[b"b".as_slice(), b"missing", b"a"]),
        vec![Some(b"second".to_vec()), None, Some(b"first".to_vec()),]
    );
    assert!(store.multi_get::<&[u8]>(&[]).is_empty());
}
