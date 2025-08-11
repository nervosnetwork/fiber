use ckb_types::packed;
use ractor::port::OutputPortSubscriber;

use super::{InvoiceUpdatedPayload, PaymentUpdatedPayload, StorePublisher, StoreUpdatedEvent};
#[cfg(feature = "watchtower")]
use crate::watchtower::{WatchtowerStore, WatchtowerStoreDeref};
use crate::{
    fiber::{
        channel::{ChannelActorStateStore, ChannelActorStateStoreDeref},
        gossip::{GossipMessageStore, GossipMessageStoreDeref},
        graph::NetworkGraphStateStore,
        history::{Direction, TimedResult},
        network::{NetworkActorStateStore, NetworkActorStateStoreDeref},
        payment::{Attempt, AttemptStatus, PaymentSession, PaymentStatus},
        types::Hash256,
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore, PreimageStore},
};

#[derive(Clone, Debug)]
pub struct StoreWithPubSub<S> {
    pub(crate) inner: S,
    publisher: StorePublisher,
}

impl<S> StoreWithPubSub<S> {
    pub fn new(store: S) -> Self {
        Self::new_with_publisher(store, StorePublisher::default())
    }

    pub fn new_with_publisher(store: S, publisher: StorePublisher) -> Self {
        Self {
            inner: store,
            publisher,
        }
    }

    pub(crate) fn publish(&self, event: StoreUpdatedEvent) {
        self.publisher.publish(event);
    }

    pub fn subscribe(&self, subscriber: OutputPortSubscriber<StoreUpdatedEvent>) {
        self.publisher.subscribe(subscriber);
    }
}

impl<T: NetworkActorStateStore> NetworkActorStateStoreDeref for StoreWithPubSub<T> {
    type Target = T;

    fn network_actor_state_store_deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: ChannelActorStateStore> ChannelActorStateStoreDeref for StoreWithPubSub<T> {
    type Target = T;

    fn channel_actor_state_store_deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<S> InvoiceStore for StoreWithPubSub<S>
where
    S: InvoiceStore,
{
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        self.inner.get_invoice(id)
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let invoice_hash = *invoice.payment_hash();
        self.inner.insert_invoice(invoice, preimage)?;

        self.publish(StoreUpdatedEvent::new_invoice_updated_event(
            invoice_hash,
            InvoiceUpdatedPayload::Open,
        ));
        Ok(())
    }

    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: CkbInvoiceStatus,
    ) -> Result<(), InvoiceError> {
        let _span = tracing::info_span!("update_invoice_status", invoice_hash = ?id).entered();

        self.inner.update_invoice_status(id, status)?;
        let payload_opt = status.try_into().ok().or_else(|| match status {
            CkbInvoiceStatus::Received => {
                // TODO: We should save received amount to the store. This is useful to
                // determine if the invoice is fully paid.
                // Currently we assume that the invoice is fully paid if the status is
                // Received.
                let payload_opt =
                    self.get_invoice(id)
                        .and_then(|invoice| invoice.amount)
                        .map(|amount| InvoiceUpdatedPayload::Received {
                            amount,
                            is_finished: true,
                        });
                if payload_opt.is_none() {
                    tracing::error!("Fail to get payment amount for a received invoice");
                }
                payload_opt
            }
            _ => {
                tracing::error!(
                    "Expect convert CkbInvoiceStatus {} to InvoiceUpdatedPayload",
                    status
                );
                None
            }
        });
        if let Some(payload) = payload_opt {
            self.publish(StoreUpdatedEvent::new_invoice_updated_event(*id, payload))
        }
        Ok(())
    }

    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus> {
        self.inner.get_invoice_status(id)
    }
}

// The PaymentUpdatedEvent requires preimage when the payment status is Received. Hooks are added
// in both `insert_preimage` and `insert_payment_session` so App does not need to worry about
// sequence to update the store.
impl<S> StoreWithPubSub<S>
where
    S: NetworkGraphStateStore + PreimageStore,
{
    fn publish_payment_updated_event_when_inserting_payment_session(
        &self,
        payment_hash: Hash256,
        status: PaymentStatus,
    ) {
        let payload_opt = status.try_into().ok().or_else(|| match status {
            // If preimage is not available, defer the notification on `insert_preimage`.
            // See `publish_payment_updated_event_when_removing_preimage`.
            PaymentStatus::Success => self
                .get_preimage(&payment_hash)
                .map(|preimage| PaymentUpdatedPayload::Success { preimage }),
            _ => {
                tracing::error!(
                    "Expect convert PaymentStatus {:?} to PaymentUpdatedPayload",
                    status
                );
                None
            }
        });
        if let Some(payload) = payload_opt {
            self.publish(StoreUpdatedEvent::new_payment_updated_event(
                payment_hash,
                payload,
            ))
        }
    }

    fn publish_payment_updated_event_when_removing_preimage(
        &self,
        payment_hash: Hash256,
        preimage: Hash256,
    ) {
        // Check whether the payment session status is Success or Inflight.
        // TODO: It is tricky to publish the success payment session status event when removing the
        // preimage from the store. This is the last chance since channel actor automatically clean the preimage before the
        // payment session is marked as success.
        if self
            .inner
            .get_payment_session(payment_hash)
            .is_some_and(|session| {
                matches!(
                    session.status,
                    PaymentStatus::Inflight | PaymentStatus::Success
                )
            })
        {
            self.publish(StoreUpdatedEvent::new_payment_updated_event(
                payment_hash,
                PaymentUpdatedPayload::Success { preimage },
            ))
        }
    }
}

impl<T> PreimageStore for StoreWithPubSub<T>
where
    T: NetworkGraphStateStore + PreimageStore,
{
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        self.inner.insert_preimage(payment_hash, preimage);
    }

    fn remove_preimage(&self, payment_hash: &Hash256) {
        if let Some(preimage) = self.inner.get_preimage(payment_hash) {
            self.publish_payment_updated_event_when_removing_preimage(*payment_hash, preimage);
        }
        self.inner.remove_preimage(payment_hash);
    }

    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        self.inner.get_preimage(payment_hash)
    }
}

impl<S> NetworkGraphStateStore for StoreWithPubSub<S>
where
    S: NetworkGraphStateStore + PreimageStore,
{
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        self.inner.get_payment_session(payment_hash)
    }

    fn get_payment_sessions_with_status(&self, status: PaymentStatus) -> Vec<PaymentSession> {
        self.inner.get_payment_sessions_with_status(status)
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        let payment_hash = session.payment_hash();
        let status = session.status;

        self.inner.insert_payment_session(session);
        self.publish_payment_updated_event_when_inserting_payment_session(payment_hash, status);
    }

    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: packed::OutPoint,
        direction: Direction,
        result: TimedResult,
    ) {
        self.inner
            .insert_payment_history_result(channel_outpoint, direction, result)
    }

    fn get_payment_history_results(&self) -> Vec<(packed::OutPoint, Direction, TimedResult)> {
        self.inner.get_payment_history_results()
    }

    fn remove_channel_history(&mut self, channel_outpoint: &packed::OutPoint) {
        self.inner.remove_channel_history(channel_outpoint);
    }

    fn get_attempt(&self, payment_hash: Hash256, attempt_id: u64) -> Option<Attempt> {
        self.inner.get_attempt(payment_hash, attempt_id)
    }

    fn insert_attempt(&self, attempt: Attempt) {
        self.inner.insert_attempt(attempt);
    }

    fn get_attempts(&self, payment_hash: Hash256) -> Vec<Attempt> {
        self.inner.get_attempts(payment_hash)
    }

    fn delete_attempts(&self, payment_hash: Hash256) {
        self.inner.delete_attempts(payment_hash);
    }

    fn get_attempts_with_statuses(&self, status: &[AttemptStatus]) -> Vec<Attempt> {
        self.inner.get_attempts_with_statuses(status)
    }
}

#[cfg(feature = "watchtower")]
impl<T: WatchtowerStore> WatchtowerStoreDeref for StoreWithPubSub<T> {
    type Target = T;

    fn watchtower_store_deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: GossipMessageStore> GossipMessageStoreDeref for StoreWithPubSub<T> {
    type Target = T;

    fn gossip_message_store_deref(&self) -> &Self::Target {
        &self.inner
    }
}
