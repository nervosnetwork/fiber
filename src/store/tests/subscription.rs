use std::{marker::PhantomData, sync::Arc, time::Duration};

use ractor::{async_trait, Actor, ActorCell, ActorProcessingErr, ActorRef};
use tokio::sync::Mutex;

use crate::{
    fiber::{
        graph::PaymentSessionStatus,
        network::SendPaymentCommand,
        tests::test_utils::{
            create_n_nodes_with_index_and_amounts_with_established_channel, init_tracing,
            HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
        },
    },
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder},
    store::subscription::{InvoiceUpdate, PaymentUpdate},
};

#[derive(Debug, Clone)]
pub struct MessageQueue<T> {
    queue: Arc<Mutex<Vec<T>>>,
}

impl<T> MessageQueue<T> {
    pub fn new() -> Self {
        MessageQueue {
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn push(&self, update: T) {
        self.queue.lock().await.push(update);
    }

    pub async fn pop(&self) -> Option<T> {
        self.queue.lock().await.pop()
    }
}

// A mock subscriber to trace which store update subscription event has been received.
pub struct MockSubscriber<T> {
    pub message_queue: MessageQueue<T>,
    pub actor: ActorRef<T>,
}

pub struct MockActor<T> {
    _phantom: PhantomData<T>,
}

impl<T> MockActor<T> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

pub struct MockActorState<T> {
    pub message_queue: MessageQueue<T>,
}

impl<T> MockActorState<T> {
    fn new(queue: MessageQueue<T>) -> Self {
        MockActorState {
            message_queue: queue,
        }
    }
}

// Create functions in `impl Actor for MockActor<T>` and `impl MockSubscriber<T>` automatically
// for any type `T`. This is needed because we can't `impl Actor for MockActor<T>` directly.
macro_rules! impl_mock_actor {
    ($type:ident) => {
        #[async_trait]
        impl Actor for MockActor<$type> {
            type Msg = $type;
            type State = MockActorState<$type>;
            type Arguments = MessageQueue<$type>;

            async fn pre_start(
                &self,
                _myself: ActorRef<Self::Msg>,
                queue: Self::Arguments,
            ) -> Result<Self::State, ActorProcessingErr> {
                Ok(Self::State::new(queue))
            }

            async fn handle(
                &self,
                _myself: ActorRef<Self::Msg>,
                message: Self::Msg,
                state: &mut Self::State,
            ) -> Result<(), ActorProcessingErr> {
                state.message_queue.push(message).await;
                Ok(())
            }
        }

        impl MockSubscriber<$type> {
            pub async fn new(supervisor: Option<ActorCell>) -> Self {
                let queue = MessageQueue::new();
                let a: MockActor<$type> = MockActor::new();
                let actor = match supervisor {
                    Some(supervisor) => {
                        Actor::spawn_linked(None, a, queue.clone(), supervisor)
                            .await
                            .expect("spawn actor success")
                            .0
                    }
                    None => {
                        Actor::spawn(None, a, queue.clone())
                            .await
                            .expect("spawn actor success")
                            .0
                    }
                };
                Self {
                    message_queue: queue,
                    actor,
                }
            }
        }
    };
}

impl_mock_actor!(InvoiceUpdate);
impl_mock_actor!(PaymentUpdate);

#[tokio::test]
async fn test_payment_subscription() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 2), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        3,
        true,
    )
    .await;
    let [mut node_0, _node_1, mut node_2] = nodes.try_into().expect("3 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_2.pubkey.clone();
    let old_amount = node_2.get_local_balance_from_channel(channels[1]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage.clone())
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    node_2.insert_invoice(ckb_invoice.clone(), None);

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(100),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            hold_payment: true,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Inflight, Some(1))
        .await;

    assert_eq!(
        node_2.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Received)
    );
    let new_amount = node_2.get_local_balance_from_channel(channels[1]);
    assert_eq!(new_amount, old_amount);

    node_2
        .settle_invoice(ckb_invoice.payment_hash(), &preimage)
        .expect("settle invoice success");

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // we should never update the invoice status if there is an error
    assert_eq!(
        node_2.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Paid)
    );
    let new_amount = node_2.get_local_balance_from_channel(channels[1]);
    assert_eq!(new_amount, old_amount + 100);

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;
}
