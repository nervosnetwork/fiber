use std::sync::Arc;
use std::time::Duration;

use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::fiber_types::Hash256;
use crate::store::store_impl::{StoreChange, StoreChangePort};

/// Records the first byte of each preimage change's payment hash, in arrival order.
struct Recorder;

#[async_trait::async_trait]
impl Actor for Recorder {
    type Msg = StoreChange;
    type State = Arc<parking_lot::Mutex<Vec<u8>>>;
    type Arguments = Arc<parking_lot::Mutex<Vec<u8>>>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        seen: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(seen)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        seen: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let StoreChange::PutPreimage { payment_hash, .. } = message {
            let bytes: &[u8] = payment_hash.as_ref();
            seen.lock().push(bytes[0]);
        }
        Ok(())
    }
}

fn change(i: u8) -> StoreChange {
    StoreChange::PutPreimage {
        payment_hash: Hash256::from([i; 32]),
        payment_preimage: Hash256::from([i; 32]),
    }
}

/// The store sends a change for each write it makes. A burst larger than ractor's output
/// port buffer, sent before any subscriber runs, must still reach the subscriber in order,
/// and later changes too. The RPC subscription and the CCH actor subscribe this way.
#[tokio::test]
async fn a_burst_of_store_changes_reaches_the_subscriber_in_order() {
    let port = StoreChangePort::default();
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (recorder, _) = Actor::spawn(None, Recorder, seen.clone()).await.unwrap();
    port.subscribe(recorder);
    for i in 0..100u8 {
        port.send(change(i));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    port.send(change(200));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let want: Vec<u8> = (0..100u8).chain([200]).collect();
    assert_eq!(*seen.lock(), want);
}

/// A subscriber that stopped is dropped from the port, and the others still get changes.
#[tokio::test]
async fn a_stopped_subscriber_is_removed() {
    let port = StoreChangePort::default();
    let kept = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let gone = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (keeper, _) = Actor::spawn(None, Recorder, kept.clone()).await.unwrap();
    let (leaver, leaver_handle) = Actor::spawn(None, Recorder, gone.clone()).await.unwrap();
    port.subscribe(keeper);
    port.subscribe(leaver.clone());
    leaver.stop(None);
    leaver_handle.await.unwrap();
    port.send(change(1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(port.subscriber_count(), 1);
    assert_eq!(*kept.lock(), vec![1]);
    assert!(gone.lock().is_empty());
}
