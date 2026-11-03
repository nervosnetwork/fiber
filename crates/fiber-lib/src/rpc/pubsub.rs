/// Store change pub/sub via RPC WebSocket subscription.
///
/// This module creates a WebSocket subscription endpoint that broadcasts `StoreChange` events
/// to connected clients. This enables the CCH service (when running separately) to receive
/// store change notifications from the Fiber node.
use crate::store::store_impl::{StoreChange, StoreChangePort};

use jsonrpsee::{RpcModule, SubscriptionSink};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use std::sync::Arc;

pub struct PubSubServerActor;

#[derive(Default)]
pub struct PubSubServerState {
    sinks: Vec<SubscriptionSink>,
}

pub enum PubSubServerMessage {
    Publish(StoreChange),
    AddSink(SubscriptionSink),
}

impl From<StoreChange> for PubSubServerMessage {
    fn from(event: StoreChange) -> Self {
        PubSubServerMessage::Publish(event)
    }
}

#[async_trait::async_trait]
impl Actor for PubSubServerActor {
    type State = PubSubServerState;
    type Msg = PubSubServerMessage;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(PubSubServerState::default())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            PubSubServerMessage::AddSink(sink) => state.sinks.push(sink),
            PubSubServerMessage::Publish(event) => {
                let subscription_message =
                    serde_json::value::to_raw_value(&event).expect("serialize to JSON");
                let sinks = std::mem::take(&mut state.sinks);
                for sink in sinks {
                    if sink.send(subscription_message.clone()).await.is_ok() {
                        state.sinks.push(sink);
                    }
                }
            }
        }
        Ok(())
    }
}

const SUBSCRIBE_STORE_CHANGES_NAME: &str = "subscribe_store_changes";
const SUBSCRIBE_STORE_CHANGES_NOTIF_NAME: &str = "store_changes";
const UNSUBSCRIBE_STORE_CHANGES_NAME: &str = "unsubscribe_store_changes";

pub async fn register_pub_sub_rpc(
    modules: &mut RpcModule<()>,
    store_change_port: &Arc<StoreChangePort>,
    supervisor: ActorCell,
) -> anyhow::Result<()> {
    let (pub_sub_actor, _) =
        ractor::Actor::spawn_linked(None, PubSubServerActor, (), supervisor).await?;
    store_change_port.subscribe(pub_sub_actor.clone());
    modules.register_subscription(
        SUBSCRIBE_STORE_CHANGES_NAME,
        SUBSCRIBE_STORE_CHANGES_NOTIF_NAME,
        UNSUBSCRIBE_STORE_CHANGES_NAME,
        move |_, pending, _, _| {
            let pub_sub_actor = pub_sub_actor.clone();
            async move {
                let sink = pending.accept().await?;
                let _ = pub_sub_actor.send_message(PubSubServerMessage::AddSink(sink));
                Ok(())
            }
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
