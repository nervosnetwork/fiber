/// Store change pub/sub via RPC WebSocket subscription.
///
/// This module creates a WebSocket subscription endpoint that broadcasts `StoreChange` events
/// to connected clients. This enables the CCH service (when running separately) to receive
/// store change notifications from the Fiber node.
use crate::store::store_impl::StoreChange;

use jsonrpsee::{RpcModule, SubscriptionSink};
use ractor::{
    port::OutputPortSubscriberTrait as _, Actor, ActorCell, ActorProcessingErr, ActorRef,
    OutputPort,
};
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
    store_change_port: &Arc<OutputPort<StoreChange>>,
    supervisor: ActorCell,
) -> anyhow::Result<()> {
    let (pub_sub_actor, _) =
        ractor::Actor::spawn_linked(None, PubSubServerActor, (), supervisor).await?;
    pub_sub_actor.subscribe_to_port(store_change_port);
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
    use super::*;
    use crate::fiber_types::Hash256;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct Counter;

    #[async_trait::async_trait]
    impl Actor for Counter {
        type Msg = StoreChange;
        type State = Arc<AtomicUsize>;
        type Arguments = Arc<AtomicUsize>;

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
            _message: Self::Msg,
            seen: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// The store sends a change for each write it makes; a burst of more than the port's
    /// buffer, before the subscriber's forwarding task runs, must still reach the subscriber,
    /// and later changes too. The RPC subscription and the CCH actor subscribe this way.
    #[tokio::test]
    async fn a_burst_of_store_changes_reaches_the_subscriber() {
        let port = Arc::new(OutputPort::<StoreChange>::default());
        let seen = Arc::new(AtomicUsize::new(0));
        let (counter, _) = Actor::spawn(None, Counter, seen.clone()).await.unwrap();
        counter.subscribe_to_port(&port);
        let change = |i: u8| StoreChange::PutPreimage {
            payment_hash: Hash256::from([i; 32]),
            payment_preimage: Hash256::from([i; 32]),
        };
        for i in 0..100u8 {
            port.send(change(i));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        port.send(change(200));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 101);
    }
}
