use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

use fiber_types::{Hash256, NodeId};
use tempfile::tempdir;

use crate::watchtower::WatchtowerStore;

use super::{open_store, WatchtowerPreimageCleanupTarget};

#[test]
fn concurrent_preimage_insert_is_not_deleted_by_stale_gc_decision() {
    let path = tempdir().expect("temp directory");
    let store = open_store(path.path()).expect("open store");
    let node_id = NodeId::local();
    let payment_hash = Hash256::from([1; 32]);
    let old_preimage = Hash256::from([2; 32]);
    let new_preimage = Hash256::from([3; 32]);
    store.insert_watch_preimage(node_id.clone(), payment_hash, old_preimage);

    let concurrent_store = store.clone();
    let concurrent_node_id = node_id.clone();
    let before_insert = Arc::new(Barrier::new(2));
    let concurrent_before_insert = Arc::clone(&before_insert);
    let (finished_tx, finished_rx) = mpsc::channel();
    let insert = std::thread::spawn(move || {
        concurrent_before_insert.wait();
        concurrent_store.insert_watch_preimage(concurrent_node_id, payment_hash, new_preimage);
        finished_tx.send(()).expect("signal insert finish");
    });

    store.cleanup_unused_watch_preimages_with_hook(
        &node_id,
        WatchtowerPreimageCleanupTarget::Exact(&payment_hash),
        || {
            before_insert.wait();
            assert!(
                finished_rx
                    .recv_timeout(Duration::from_millis(100))
                    .is_err(),
                "concurrent insert must wait for the GC delete commit"
            );
        },
    );
    insert.join().expect("concurrent insert thread");

    assert_eq!(
        store.get_watch_preimage(&node_id, &payment_hash),
        Some(new_preimage),
        "GC must not delete a preimage committed after its liveness decision"
    );
}

#[test]
fn one_node_preimage_gc_does_not_block_another_node_writer() {
    let path = tempdir().expect("temp directory");
    let store = open_store(path.path()).expect("open store");
    let cleanup_node_id = NodeId::from_bytes(vec![1]);
    let writer_node_id = NodeId::from_bytes(vec![2]);
    let cleanup_payment_hash = Hash256::from([4; 32]);
    let writer_payment_hash = Hash256::from([5; 32]);
    let writer_preimage = Hash256::from([6; 32]);
    store.insert_watch_preimage(
        cleanup_node_id.clone(),
        cleanup_payment_hash,
        Hash256::from([7; 32]),
    );

    let concurrent_store = store.clone();
    let concurrent_writer_node_id = writer_node_id.clone();
    let before_insert = Arc::new(Barrier::new(2));
    let concurrent_before_insert = Arc::clone(&before_insert);
    let (finished_tx, finished_rx) = mpsc::channel();
    let insert = std::thread::spawn(move || {
        concurrent_before_insert.wait();
        concurrent_store.insert_watch_preimage(
            concurrent_writer_node_id,
            writer_payment_hash,
            writer_preimage,
        );
        finished_tx.send(()).expect("signal insert finish");
    });

    store.cleanup_unused_watch_preimages_with_hook(
        &cleanup_node_id,
        WatchtowerPreimageCleanupTarget::Exact(&cleanup_payment_hash),
        || {
            before_insert.wait();
            finished_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("another node's writer must not wait for this node's GC");
        },
    );
    insert.join().expect("concurrent insert thread");

    assert_eq!(
        store.get_watch_preimage(&writer_node_id, &writer_payment_hash),
        Some(writer_preimage),
        "another node's preimage write must complete independently"
    );
}
