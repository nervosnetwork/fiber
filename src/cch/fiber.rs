use jsonrpsee::{core::client::SubscriptionClientT, rpc_params, ws_client::WsClient};
use ractor::{ActorRef, DerivedActorRef};
use serde::de::DeserializeOwned;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::{
    fiber::{types::Hash256, NetworkActorMessage},
    store::{
        subscription::{InvoiceSubscription, InvoiceUpdate, PaymentSubscription, PaymentUpdate},
        subscription_impl::SubscriptionImpl,
        SubscriptionId,
    },
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
};

use super::CchError;

pub struct FiberBackend {
    pub subscription: FiberSubcriptionBackend,
    pub rpc: FiberRpcBackend,
}

pub type FiberRpcBackend = ActorRef<NetworkActorMessage>;

pub enum FiberSubcriptionBackend {
    InProcess(SubscriptionImpl),
    Websocket(WsClient),
}

pub enum FiberTrackingHandle {
    // We are connecting to the fiber service by an in-process actor.
    // The subscription id is used to unsubscribe the invoice.
    InProcess(SubscriptionId),
    // We are connecting to the fiber service by a websocket.
    // The cancellation token is used to cancel the subscription.
    Websocket(CancellationToken),
}

impl FiberSubcriptionBackend {
    pub async fn subscribe_invoice(
        &self,
        hash: Hash256,
        receiver: DerivedActorRef<InvoiceUpdate>,
    ) -> Result<FiberTrackingHandle, CchError> {
        match self {
            FiberSubcriptionBackend::InProcess(subscription) => Ok(FiberTrackingHandle::InProcess(
                subscription.subscribe_invoice(hash, receiver).await?,
            )),
            FiberSubcriptionBackend::Websocket(ws_client) => {
                let subscription = ws_client
                    .subscribe(
                        "subscribe_invoice",
                        rpc_params![hash],
                        "unsubscribe_invoice",
                    )
                    .await?;
                let token = process_subscription(receiver, subscription);
                Ok(FiberTrackingHandle::Websocket(token))
            }
        }
    }

    pub async fn unsubscribe_invoice(&self, handle: FiberTrackingHandle) -> Result<(), CchError> {
        match handle {
            FiberTrackingHandle::InProcess(subscription_id) => {
                match self {
                    FiberSubcriptionBackend::InProcess(subscription) => {
                        subscription.unsubscribe_invoice(subscription_id).await?;
                        Ok(())
                    }
                    _ => {
                        panic!("Trying to unsubscribe an in-process subscription with a websocket backend");
                    }
                }
            }
            FiberTrackingHandle::Websocket(token) => {
                token.cancel();
                Ok(())
            }
        }
    }

    pub async fn subscribe_payment(
        &self,
        hash: Hash256,
        receiver: DerivedActorRef<PaymentUpdate>,
    ) -> Result<FiberTrackingHandle, CchError> {
        match self {
            FiberSubcriptionBackend::InProcess(subscription) => Ok(FiberTrackingHandle::InProcess(
                subscription.subscribe_payment(hash, receiver).await?,
            )),
            FiberSubcriptionBackend::Websocket(ws_client) => {
                let subscription = ws_client
                    .subscribe(
                        "subscribe_payment",
                        rpc_params![hash],
                        "unsubscribe_payment",
                    )
                    .await?;
                let token = process_subscription(receiver, subscription);
                Ok(FiberTrackingHandle::Websocket(token))
            }
        }
    }

    pub async fn unsubscribe_payment(&self, handle: FiberTrackingHandle) -> Result<(), CchError> {
        match handle {
            FiberTrackingHandle::InProcess(subscription_id) => {
                match self {
                    FiberSubcriptionBackend::InProcess(subscription) => {
                        subscription.unsubscribe_payment(subscription_id).await?;
                        Ok(())
                    }
                    _ => {
                        panic!("Trying to unsubscribe an in-process subscription with a websocket backend");
                    }
                }
            }
            FiberTrackingHandle::Websocket(token) => {
                token.cancel();
                Ok(())
            }
        }
    }
}

fn process_subscription<T: DeserializeOwned + Send + Sync + 'static>(
    receiver: DerivedActorRef<T>,
    mut subscription: jsonrpsee::core::client::Subscription<T>,
) -> CancellationToken {
    let token = new_tokio_cancellation_token();
    let subtoken = token.child_token();
    let cloned_subtoken = subtoken.clone();
    let tracker = new_tokio_task_tracker();
    tracker.spawn(async move {
        loop {
            select! {
                _ = subtoken.cancelled() => break,
                message = subscription.next() => {
                    match message {
                        Some(Ok(message)) => {
                            if let Err(error) = receiver.send_message(message) {
                                tracing::error!(error = ?error, "Failed to send message to actor");
                                break;
                            }
                        }
                        Some(Err(err)) => {
                            tracing::error!(error = ?err, "Failed to receive message from websocket");
                            break;
                        }
                        None => {
                            tracing::error!("Websocket closed");
                            break;
                        }
                    }
                }
            }
        }
    });
    cloned_subtoken
}
