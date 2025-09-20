//! Store Pub/Sub via RPC
use super::StoreUpdatedEvent;
use crate::store::pub_sub::{StorePublisher, Subscribe};

use jsonrpsee::{
    core::client::{Subscription, SubscriptionClientT},
    rpc_params,
    ws_client::WsClientBuilder,
    RpcModule, SubscriptionSink,
};
use ractor::{port::OutputPortSubscriber, Actor, ActorCell, ActorProcessingErr, ActorRef};
use tokio::select;
use tokio_util::sync::CancellationToken;

pub struct PubSubServerActor;

#[derive(Default)]
pub struct PubSubServerState {
    sinks: Vec<SubscriptionSink>,
}

pub enum PubSubServerMessage {
    Publish(StoreUpdatedEvent),
    AddSink(SubscriptionSink),
}

impl From<StoreUpdatedEvent> for PubSubServerMessage {
    fn from(event: StoreUpdatedEvent) -> Self {
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

pub async fn register_pub_sub_rpc<S: Subscribe>(
    modules: &mut RpcModule<()>,
    publisher: &S,
    supervisor: ActorCell,
) -> anyhow::Result<()> {
    let (pub_sub_actor, _) =
        ractor::Actor::spawn_linked(None, PubSubServerActor, (), supervisor).await?;
    publisher.subscribe(Box::new(pub_sub_actor.clone()));
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

pub struct PubSubClient {
    ws_url: String,
    publisher: StorePublisher,
}

impl PubSubClient {
    pub fn new(ws_url: String) -> Self {
        Self {
            ws_url,
            publisher: Default::default(),
        }
    }

    pub fn subscribe(&self, subscriber: OutputPortSubscriber<StoreUpdatedEvent>) {
        self.publisher.subscribe(subscriber);
    }

    async fn run_inner(&self, token: &CancellationToken) -> anyhow::Result<()> {
        let client = WsClientBuilder::default().build(&self.ws_url).await?;
        let mut subscription: Subscription<StoreUpdatedEvent> = client
            .subscribe(
                SUBSCRIBE_STORE_CHANGES_NAME,
                rpc_params![],
                UNSUBSCRIBE_STORE_CHANGES_NAME,
            )
            .await?;

        loop {
            select! {
                notif = subscription.next() => {
                    match notif {
                        Some(Ok(event)) => {
                            self.publisher.publish(event);
                        },
                        Some(Err(err)) => {
                            // ignore errors from server
                            tracing::error!("unexpected store changes stream error: {}", err);
                        },
                        None => {
                            return Err(anyhow::anyhow!("reconnect"));
                        }
                    }
                }
                _ = token.cancelled() => {
                    break;
                }
            }
        }
        Ok(())
    }

    pub async fn run(self, token: CancellationToken) {
        loop {
            select! {
                result = self.run_inner(&token) => {
                    if let Err(err) = result {
                        tracing::error!(
                            "restart pub sub client to {} because of error: {}",
                            self.ws_url,
                            err
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    } else {
                        break;
                    }
                }
                _ = token.cancelled() => {
                    break;
                }
            }
        }
    }
}
