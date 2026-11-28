use std::{sync::Arc, time::Duration};

use fiber_types::{Hash256, NodeId};

use super::{SigningChange, SigningNotifications, SigningScope};

#[tokio::test]
async fn signing_events_are_shared_scoped_and_cleaned_up() {
    let hub = Arc::new(SigningNotifications::default());
    let alice = SigningScope::Channel(Some("alice".into()));
    let bob = SigningScope::Channel(Some("bob".into()));
    let mut first = hub.subscribe(alice.clone());
    let mut second = hub.subscribe(alice.clone());
    let mut others = [
        hub.subscribe(bob),
        hub.subscribe(SigningScope::Channel(None)),
        hub.subscribe(SigningScope::Watchtower(NodeId::local())),
    ];
    let topic = hub.topic(alice.clone());
    let id = Hash256::from([1; 32]);
    topic.write().publish(|| SigningChange::Removed(id));
    let first_event = first.recv().await.unwrap();
    let second_event = second.recv().await.unwrap();
    assert!(
        Arc::ptr_eq(&first_event, &second_event),
        "receivers share one payload"
    );
    for other in &mut others {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), other.recv())
                .await
                .is_err()
        );
    }
    drop((first, second, others, topic));
    assert!(hub.0.lock().is_empty());
    let unobserved = hub.topic(alice);
    unobserved
        .write()
        .publish(|| panic!("unobserved scopes build no events"));
    drop(unobserved);
    assert!(hub.0.lock().is_empty());
}

#[tokio::test]
async fn snapshot_excludes_writes_and_discards_only_older_events() {
    let hub = Arc::new(SigningNotifications::default());
    let scope = SigningScope::Channel(None);
    let mut changes = hub.subscribe(scope.clone());
    let topic = hub.topic(scope);
    let old = Hash256::from([1; 32]);
    let new = Hash256::from([2; 32]);
    topic.write().publish(|| SigningChange::Removed(old));
    let (started, wait_started) = std::sync::mpsc::channel();
    let writer_topic = topic.clone();
    let writer = changes.snapshot(|| {
        let writer = std::thread::spawn(move || {
            started.send(()).unwrap();
            writer_topic.write().publish(|| SigningChange::Removed(new));
        });
        wait_started.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            topic.sender.try_lock().is_none(),
            "initial read holds the write boundary"
        );
        writer
    });
    writer.join().unwrap();
    assert_eq!(changes.recv().await.unwrap().channel_id(), new);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), changes.recv())
            .await
            .is_err()
    );
}
