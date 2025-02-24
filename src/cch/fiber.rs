use jsonrpsee::{
    core::client::{ClientT, SubscriptionClientT},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    ws_client::{WsClient, WsClientBuilder},
};
use ractor::{call, ActorRef, DerivedActorRef};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::{
    errors::ALREADY_EXISTS_DESCRIPTION,
    fiber::{
        network::{NewInvoiceCommand, SendPaymentCommand},
        types::Hash256,
        NetworkActorCommand, NetworkActorMessage,
    },
    invoice::CkbInvoice,
    rpc::{
        invoice::{InvoiceResult, NewInvoiceParams, SettleInvoiceParams, SettleInvoiceResult},
        payment::{GetPaymentCommandResult, SendPaymentCommandParams},
        pubsub::{
            SUBSCRIBE_INVOICE_UPDATE_METHOD, SUBSCRIBE_PAYMENT_UPDATE_METHOD,
            UNSUBSCRIBE_INVOICE_UPDATE_METHOD, UNSUBSCRIBE_PAYMENT_UPDATE_METHOD,
        },
    },
    store::{
        subscription::{
            InvoiceSubscription, InvoiceUpdate, PaymentState, PaymentSubscription, PaymentUpdate,
        },
        subscription_impl::SubscriptionImpl,
        SubscriptionId,
    },
    tasks::{new_tokio_cancellation_child_token, new_tokio_task_tracker},
};

use super::{order::FiberPaymentUpdate, CchError};

pub enum FiberBackend {
    InProcess(InProcessFiberBackend),
    Http(HttpBackend),
}

pub struct InProcessFiberBackend {
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub subscription: SubscriptionImpl,
}

impl InProcessFiberBackend {
    pub fn new(
        network_actor: ActorRef<NetworkActorMessage>,
        subscription: SubscriptionImpl,
    ) -> Self {
        Self {
            network_actor,
            subscription,
        }
    }
}

#[derive(Default)]
pub struct HttpBackend {
    pub url: String,
    pub ws_client: Option<WsClient>,
    pub http_client: Option<HttpClient>,
}

impl HttpBackend {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            ws_client: None,
            http_client: None,
        }
    }

    pub async fn connect_ws(&mut self) -> Result<(), jsonrpsee::core::ClientError> {
        self.get_ws_client().await?;
        Ok(())
    }

    pub async fn get_ws_client(&mut self) -> Result<&WsClient, jsonrpsee::core::ClientError> {
        self.ws_client = Some(WsClientBuilder::default().build(self.ws_url()).await?);
        Ok(self.ws_client.as_ref().expect("Created ws client above"))
    }

    pub async fn get_http_client(&self) -> Result<HttpClient, jsonrpsee::core::ClientError> {
        HttpClientBuilder::default().build(self.http_url())
    }

    async fn call<T, R>(&self, method: &str, params: T) -> Result<R, jsonrpsee::core::ClientError>
    where
        T: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        let client = self.get_http_client().await?;
        client.request(method, rpc_params!(params)).await
    }

    pub fn http_url(&self) -> &str {
        &self.url
    }

    pub fn ws_url(&self) -> String {
        format!("ws{}", self.url.trim_start_matches("http"))
    }
}

pub enum FiberTrackingHandle {
    // We are connecting to the fiber service by an in-process actor.
    // The subscription id is used to unsubscribe the invoice.
    InProcess(SubscriptionId),
    // We are connecting to the fiber service by a websocket.
    // The cancellation token is used to cancel the subscription.
    Websocket(CancellationToken),
}

impl FiberBackend {
    pub async fn subscribe_invoice(
        &mut self,
        hash: Hash256,
        receiver: DerivedActorRef<InvoiceUpdate>,
    ) -> Result<FiberTrackingHandle, CchError> {
        match self {
            FiberBackend::InProcess(backend) => Ok(FiberTrackingHandle::InProcess(
                backend
                    .subscription
                    .subscribe_invoice(hash, receiver)
                    .await?,
            )),
            FiberBackend::Http(backend) => {
                let ws_client = backend.get_ws_client().await?;
                let subscription = ws_client
                    .subscribe(
                        SUBSCRIBE_INVOICE_UPDATE_METHOD,
                        rpc_params![hash],
                        UNSUBSCRIBE_INVOICE_UPDATE_METHOD,
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
                    FiberBackend::InProcess(backend) => {
                        backend
                            .subscription
                            .unsubscribe_invoice(subscription_id)
                            .await?;
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
        &mut self,
        hash: Hash256,
        receiver: DerivedActorRef<PaymentUpdate>,
    ) -> Result<FiberTrackingHandle, CchError> {
        match self {
            FiberBackend::InProcess(backend) => Ok(FiberTrackingHandle::InProcess(
                backend
                    .subscription
                    .subscribe_payment(hash, receiver)
                    .await?,
            )),
            FiberBackend::Http(backend) => {
                let ws_client = backend.get_ws_client().await?;
                let subscription = ws_client
                    .subscribe(
                        SUBSCRIBE_PAYMENT_UPDATE_METHOD,
                        rpc_params![hash],
                        UNSUBSCRIBE_PAYMENT_UPDATE_METHOD,
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
                    FiberBackend::InProcess(backend) => {
                        backend
                            .subscription
                            .unsubscribe_payment(subscription_id)
                            .await?;
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

    pub async fn new_invoice<I: Into<NewInvoiceCommand>>(
        &mut self,
        request: I,
    ) -> Result<CkbInvoice, CchError> {
        let request = request.into();
        match self {
            FiberBackend::InProcess(backend) => {
                let message = |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::NewInvoice(
                        request, rpc_reply,
                    ))
                };

                Ok(call!(&backend.network_actor, message).expect("call actor")?)
            }
            FiberBackend::Http(backend) => backend
                .call("new_invoice", NewInvoiceParams::from(request))
                .await
                .map(|r: InvoiceResult| r.invoice)
                .map_err(Into::into),
        }
    }

    pub async fn pay_invoice(
        &mut self,
        invoice: &CkbInvoice,
    ) -> Result<Option<FiberPaymentUpdate>, CchError> {
        let payment_hash = *invoice.payment_hash();
        match self {
            FiberBackend::InProcess(backend) => {
                let message = |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                        SendPaymentCommand {
                            invoice: Some(invoice.to_string()),
                            ..Default::default()
                        },
                        rpc_reply,
                    ))
                };

                let send_payment_response =
                    match call!(backend.network_actor, message).expect("call actor") {
                        Ok(tlc_response) => tlc_response,
                        Err(err) if err.contains(ALREADY_EXISTS_DESCRIPTION) => return Ok(None),
                        Err(err) => return Err(CchError::SendFiberPaymentError(err.to_string())),
                    };
                Ok(
                    Option::<PaymentState>::from(send_payment_response.status).map(|state| {
                        FiberPaymentUpdate {
                            hash: payment_hash,
                            state,
                        }
                    }),
                )
            }
            FiberBackend::Http(backend) => backend
                .call(
                    "send_payment",
                    SendPaymentCommandParams {
                        invoice: Some(invoice.to_string()),
                        ..Default::default()
                    },
                )
                .await
                .map(|response: GetPaymentCommandResult| {
                    Option::<PaymentState>::from(response.status).map(|state| PaymentUpdate {
                        hash: payment_hash,
                        state,
                    })
                })
                .map_err(Into::into),
        }
    }

    pub async fn settle_invoice(
        &mut self,
        invoice: &CkbInvoice,
        preimage: Hash256,
    ) -> Result<(), CchError> {
        match self {
            FiberBackend::InProcess(backend) => {
                let message = move |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                        *invoice.payment_hash(),
                        preimage,
                        rpc_reply,
                    ))
                };

                call!(&backend.network_actor, message).expect("call actor")?;
                Ok(())
            }
            FiberBackend::Http(backend) => backend
                .call(
                    "settle_invoice",
                    SettleInvoiceParams {
                        payment_hash: *invoice.payment_hash(),
                        payment_preimage: preimage,
                    },
                )
                .await
                .map(|_: SettleInvoiceResult| ())
                .map_err(Into::into),
        }
    }
}

fn process_subscription<T: DeserializeOwned + Send + Sync + 'static>(
    receiver: DerivedActorRef<T>,
    mut subscription: jsonrpsee::core::client::Subscription<T>,
) -> CancellationToken {
    let token = new_tokio_cancellation_child_token();
    let cloned_token = token.clone();
    let tracker = new_tokio_task_tracker();
    tracker.spawn(async move {
        loop {
            select! {
                _ = token.cancelled() => break,
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
    cloned_token
}
