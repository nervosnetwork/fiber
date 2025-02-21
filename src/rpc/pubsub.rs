use std::sync::Arc;

use jsonrpsee::{core::StringError, types::Params, PendingSubscriptionSink, RpcModule};

use crate::store::subscription_impl::SubscriptionImpl;

pub(crate) async fn start_pubsub_server<C: Send + Sync + 'static>(
    module: &mut RpcModule<C>,
    subscription_impl: SubscriptionImpl,
) {
    let subscription_impl_1 = subscription_impl.clone();
    let subscription_impl_2 = subscription_impl.clone();
    module
        .register_subscription(
            "subscribe_invoice_update",
            "subscribe_invoice_update",
            "unsubscribe_invoice_update",
            move |params, pending_sink, context| {
                subscribe_invoice_update(subscription_impl_1.clone(), params, pending_sink, context)
            },
        )
        .expect("register subscribe_invoice_update");
    module
        .register_subscription(
            "subscribe_payment_update",
            "subscribe_payment_update",
            "unsubscribe_payment_update",
            move |params, pending_sink, context| {
                subscribe_payment_update(subscription_impl_2.clone(), params, pending_sink, context)
            },
        )
        .expect("register subscribe_payment_update");
}

async fn subscribe_invoice_update<C: Send + Sync + 'static>(
    _subscription_impl: SubscriptionImpl,
    _params: Params<'static>,
    _pending_sink: PendingSubscriptionSink,
    _context: Arc<C>,
) -> Result<(), StringError> {
    Ok(())
}

async fn subscribe_payment_update<C: Send + Sync + 'static>(
    _subscription_impl: SubscriptionImpl,
    _params: Params<'static>,
    _pending_sink: PendingSubscriptionSink,
    _context: Arc<C>,
) -> Result<(), StringError> {
    Ok(())
}
