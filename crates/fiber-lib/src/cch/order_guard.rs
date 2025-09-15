//! Order guard takes care of the order lifecycle.

use std::{
    collections::BinaryHeap,
    marker::PhantomData,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ractor::{Actor, ActorProcessingErr, ActorRef, DerivedActorRef};

use crate::{
    cch::{CchOrder, CchOrderStore},
    fiber::types::Hash256,
};

pub struct CchOrderGuardActor<S>(PhantomData<S>);

impl<S> Default for CchOrderGuardActor<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// The events that CchOrderGuardActor can trigger
pub enum CchOrderGuardEvent {
    OrderLoaded(CchOrder),
}

pub struct CchOrderGuardArgs<S> {
    pub watcher: DerivedActorRef<CchOrderGuardEvent>,
    /// Number of seconds that an inactive order that should be deleted from the database. The time
    /// counts from the expire time, a.k.a, order created time plus expires ttl.
    pub purge_ttl: u64,
    pub store: S,
}

pub struct CchOrderPurgeItem {
    payment_hash: Hash256,
    purge_at: u64,
}

impl Ord for CchOrderPurgeItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse the order to make the BinaryHeap a min-heap based on purge_at
        other.purge_at.cmp(&self.purge_at)
    }
}
impl PartialOrd for CchOrderPurgeItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Eq for CchOrderPurgeItem {}
impl PartialEq for CchOrderPurgeItem {
    fn eq(&self, other: &Self) -> bool {
        self.purge_at == other.purge_at
    }
}

pub struct CchOrderGuardState<S> {
    watcher: DerivedActorRef<CchOrderGuardEvent>,
    /// Number of seconds that an inactive order that should be deleted from the database. The time
    /// counts from the expire time, a.k.a, order created time plus expires ttl.
    purge_ttl: u64,
    store: S,
    purge_heap: BinaryHeap<CchOrderPurgeItem>,
    interval_purging_started: bool,
}

pub enum CchOrderGuardMessage {
    /// The order is Succeeded or Failed, and should be purged later.
    DeactivateOrder {
        payment_hash: Hash256,
        // order.created_at + order.expires_after
        expires_at: u64,
    },
    /// Load orders from store
    LoadOrders,
    /// Tick to check whether need to purge orders
    PurgeOrders,
}

#[async_trait::async_trait]
impl<S: CchOrderStore + Send + Sync + 'static> Actor for CchOrderGuardActor<S> {
    type Msg = CchOrderGuardMessage;
    type State = CchOrderGuardState<S>;
    type Arguments = CchOrderGuardArgs<S>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_message(CchOrderGuardMessage::LoadOrders)?;
        Ok(Self::State {
            watcher: args.watcher,
            purge_ttl: args.purge_ttl,
            store: args.store,
            purge_heap: Default::default(),
            interval_purging_started: false,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.handle(myself, message).await
    }
}

impl<S: CchOrderStore + Send + Sync + 'static> CchOrderGuardState<S> {
    async fn handle(
        &mut self,
        myself: ActorRef<CchOrderGuardMessage>,
        message: CchOrderGuardMessage,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchOrderGuardMessage::LoadOrders => {
                self.load_orders().await?;
                // Purge orders every hour
                if !self.interval_purging_started {
                    self.interval_purging_started = true;
                    myself.send_interval(Duration::from_secs(60 * 60), || {
                        CchOrderGuardMessage::PurgeOrders
                    });
                }
                Ok(())
            }
            CchOrderGuardMessage::DeactivateOrder {
                payment_hash,
                expires_at,
            } => {
                self.deactivate_order(payment_hash, expires_at).await;
                Ok(())
            }
            CchOrderGuardMessage::PurgeOrders => self.purge_orders().await,
        }
    }

    async fn load_orders(&mut self) -> Result<(), ActorProcessingErr> {
        // Assume that all order keys can be loaded into memory.
        let keys: Vec<_> = self.store.get_cch_order_keys_iter().into_iter().collect();
        for key in keys.into_iter() {
            // Ignore not found orders
            if let Ok(order) = self.store.get_cch_order(&key) {
                if order.status.is_active() {
                    self.watcher
                        .send_message(CchOrderGuardEvent::OrderLoaded(order))?;
                } else {
                    self.deactivate_order(
                        order.payment_hash,
                        order.created_at + order.expires_after,
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn deactivate_order(&mut self, payment_hash: Hash256, expires_at: u64) {
        self.purge_heap.push(CchOrderPurgeItem {
            payment_hash,
            purge_at: expires_at + self.purge_ttl,
        });
    }

    async fn purge_orders(&mut self) -> Result<(), ActorProcessingErr> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs();
        // get current time
        while let Some(payment_hash) = self
            .purge_heap
            .peek()
            .and_then(|item| (item.purge_at <= now).then_some(item.payment_hash))
        {
            self.store.delete_cch_order(&payment_hash);
            self.purge_heap.pop();
        }
        Ok(())
    }
}
