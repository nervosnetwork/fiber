use std::collections::HashMap;

#[cfg(test)]
use std::{future::Future, pin::Pin, sync::Arc};

use ckb_types::{core::TransactionView, packed};
use futures::FutureExt;
use ractor::{
    concurrency::{Duration, JoinHandle},
    Actor, ActorProcessingErr, ActorRef, RpcReplyPort,
};

/// A confirmed transaction that spends the watched CKB outpoint.
#[derive(Clone, Debug)]
pub struct CkbOutPointSpendTracingResult {
    pub outpoint: packed::OutPoint,
    pub spending_transaction: TransactionView,
    pub input_index: usize,
    /// First transaction input in the exact lock-script group.
    pub script_group_input_index: usize,
    pub block_number: u64,
}

/// A request to watch an exact CKB outpoint until its committed spender is found.
#[derive(Debug)]
pub struct CkbOutPointSpendTracer {
    pub outpoint: packed::OutPoint,
    pub lock_script: packed::Script,
    pub confirmations: u64,
    /// Receives the eventual committed spend discovery result.
    pub callback: RpcReplyPort<Result<CkbOutPointSpendTracingResult, String>>,
    /// Acknowledges whether this waiter was installed without consuming `callback`.
    pub registration: RpcReplyPort<Result<(), String>>,
}

pub struct CkbOutPointSpendTracingArguments {
    pub rpc_url: String,
    /// Tracing uses polling until CKB offers a suitable push mechanism.
    pub polling_interval: Duration,
}

#[cfg(test)]
type DiscoveryFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<CkbOutPointSpendTracingResult>, String>> + Send + 'static,
    >,
>;
#[cfg(test)]
type DiscoveryFn = Arc<
    dyn Fn(String, packed::Script, packed::OutPoint, u64) -> DiscoveryFuture
        + Send
        + Sync
        + 'static,
>;

pub struct CkbOutPointSpendTracingActor {
    #[cfg(test)]
    discovery: Option<DiscoveryFn>,
}

impl CkbOutPointSpendTracingActor {
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            discovery: None,
        }
    }

    #[cfg(test)]
    fn with_discovery<F>(discovery: F) -> Self
    where
        F: Fn(String, packed::Script, packed::OutPoint, u64) -> DiscoveryFuture
            + Send
            + Sync
            + 'static,
    {
        Self {
            discovery: Some(Arc::new(discovery)),
        }
    }
}

impl Default for CkbOutPointSpendTracingActor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum CkbOutPointSpendTracingMessage {
    CreateTracer(CkbOutPointSpendTracer),
    RemoveTracers(packed::OutPoint),
    RunTracers,
    ReportTracingResult {
        outpoint: packed::OutPoint,
        generation: u64,
        result: Result<Option<CkbOutPointSpendTracingResult>, String>,
    },
}

#[derive(Debug)]
struct OutPointTracerGroup {
    lock_script: packed::Script,
    confirmations: u64,
    callbacks: Vec<RpcReplyPort<Result<CkbOutPointSpendTracingResult, String>>>,
    generation: u64,
    task: Option<JoinHandle<()>>,
}

pub struct CkbOutPointSpendTracingState {
    rpc_url: String,
    tracers: HashMap<packed::OutPoint, OutPointTracerGroup>,
    next_generation: u64,
    #[cfg(test)]
    discovery: Option<DiscoveryFn>,
}

#[async_trait::async_trait]
impl Actor for CkbOutPointSpendTracingActor {
    type Msg = CkbOutPointSpendTracingMessage;
    type State = CkbOutPointSpendTracingState;
    type Arguments = CkbOutPointSpendTracingArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        arguments: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_interval(arguments.polling_interval, || {
            CkbOutPointSpendTracingMessage::RunTracers
        });
        Ok(CkbOutPointSpendTracingState {
            rpc_url: arguments.rpc_url,
            tracers: HashMap::new(),
            next_generation: 0,
            #[cfg(test)]
            discovery: self.discovery.clone(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CkbOutPointSpendTracingMessage::CreateTracer(tracer) => {
                state.create_tracer(myself, tracer)
            }
            CkbOutPointSpendTracingMessage::RemoveTracers(outpoint) => {
                if let Some(mut group) = state.tracers.remove(&outpoint) {
                    if let Some(task) = group.task.as_mut() {
                        task.abort();
                    }
                }
                Ok(())
            }
            CkbOutPointSpendTracingMessage::RunTracers => state.run_tracers(myself),
            CkbOutPointSpendTracingMessage::ReportTracingResult {
                outpoint,
                generation,
                result,
            } => {
                state.report_tracing_result(outpoint, generation, result);
                Ok(())
            }
        }
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        for group in state.tracers.values_mut() {
            if let Some(task) = group.task.as_mut() {
                task.abort();
            }
        }
        Ok(())
    }
}

impl CkbOutPointSpendTracingState {
    fn create_tracer(
        &mut self,
        myself: ActorRef<CkbOutPointSpendTracingMessage>,
        tracer: CkbOutPointSpendTracer,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(group) = self.tracers.get_mut(&tracer.outpoint) {
            if group.lock_script == tracer.lock_script
                && group.confirmations == tracer.confirmations
            {
                group.callbacks.push(tracer.callback);
                let _ = tracer.registration.send(Ok(()));
            } else {
                let _ = tracer.registration.send(Err(
                    "conflicting lock script or confirmation policy for watched outpoint"
                        .to_string(),
                ));
            }
            return Ok(());
        }

        self.next_generation = self.next_generation.wrapping_add(1);
        let outpoint = tracer.outpoint;
        self.tracers.insert(
            outpoint,
            OutPointTracerGroup {
                lock_script: tracer.lock_script,
                confirmations: tracer.confirmations,
                callbacks: vec![tracer.callback],
                generation: self.next_generation,
                task: None,
            },
        );
        let _ = tracer.registration.send(Ok(()));
        myself.send_message(CkbOutPointSpendTracingMessage::RunTracers)?;
        Ok(())
    }

    fn run_tracers(
        &mut self,
        myself: ActorRef<CkbOutPointSpendTracingMessage>,
    ) -> Result<(), ActorProcessingErr> {
        for (outpoint, group) in &mut self.tracers {
            if group.task.is_some() {
                continue;
            }
            group.task = Some(
                PollTask {
                    actor: myself.clone(),
                    rpc_url: self.rpc_url.clone(),
                    outpoint: outpoint.clone(),
                    lock_script: group.lock_script.clone(),
                    confirmations: group.confirmations,
                    generation: group.generation,
                    #[cfg(test)]
                    discovery: self.discovery.clone(),
                }
                .spawn(),
            );
        }
        Ok(())
    }

    fn report_tracing_result(
        &mut self,
        outpoint: packed::OutPoint,
        generation: u64,
        result: Result<Option<CkbOutPointSpendTracingResult>, String>,
    ) {
        let Some(group) = self.tracers.get_mut(&outpoint) else {
            return;
        };
        if group.generation != generation {
            return;
        }
        group.task = None;
        match result {
            Ok(Some(result)) => {
                let group = self
                    .tracers
                    .remove(&outpoint)
                    .expect("tracer group exists while reporting result");
                for callback in group.callbacks {
                    let _ = callback.send(Ok(result.clone()));
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                "Failed to discover committed spender for CKB outpoint {}: {}",
                outpoint,
                error
            ),
        }
    }
}

struct PollTask {
    actor: ActorRef<CkbOutPointSpendTracingMessage>,
    rpc_url: String,
    outpoint: packed::OutPoint,
    lock_script: packed::Script,
    confirmations: u64,
    generation: u64,
    #[cfg(test)]
    discovery: Option<DiscoveryFn>,
}

impl PollTask {
    fn spawn(self) -> JoinHandle<()> {
        ractor::concurrency::spawn(async move {
            // A panic in the discovery future must not leave the task handle in
            // the `Some` state forever, otherwise the group is never re-polled.
            // Catch it and convert it into a descriptive error that clears the
            // task handle so the next poll retries.
            let result = std::panic::AssertUnwindSafe(self.discover())
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| Err(panic_message(&payload)));
            let _ = self
                .actor
                .send_message(CkbOutPointSpendTracingMessage::ReportTracingResult {
                    outpoint: self.outpoint,
                    generation: self.generation,
                    result,
                });
        })
    }

    async fn discover(&self) -> Result<Option<CkbOutPointSpendTracingResult>, String> {
        #[cfg(test)]
        if let Some(discovery) = &self.discovery {
            return discovery(
                self.rpc_url.clone(),
                self.lock_script.clone(),
                self.outpoint.clone(),
                self.confirmations,
            )
            .await;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            return crate::ckb::client::find_committed_outpoint_spend(
                &self.rpc_url,
                &self.lock_script,
                &self.outpoint,
                self.confirmations,
            )
            .await
            .map(|spend| {
                spend.map(|spend| CkbOutPointSpendTracingResult {
                    outpoint: self.outpoint.clone(),
                    spending_transaction: spend.transaction,
                    input_index: spend.input_index,
                    script_group_input_index: spend.script_group_input_index,
                    block_number: spend.block_number,
                })
            })
            .map_err(|error| error.to_string());
        }

        #[cfg(target_arch = "wasm32")]
        {
            let _ = (&self.rpc_url, &self.lock_script, self.confirmations);
            Err("CKB outpoint spend discovery is unavailable on WASM".to_string())
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use ckb_types::{
        core::TransactionBuilder,
        packed,
        prelude::{Builder, Entity, Pack},
    };
    use ractor::{concurrency::Duration, Actor, RpcReplyPort};
    use tokio::{
        sync::{oneshot, Notify},
        time::timeout,
    };

    use super::*;

    fn outpoint(index: u32) -> packed::OutPoint {
        packed::OutPoint::new_builder()
            .tx_hash([index as u8; 32].pack())
            .index(index)
            .build()
    }

    fn lock_script() -> packed::Script {
        packed::Script::new_builder()
            .code_hash([7; 32].pack())
            .build()
    }

    fn conflicting_lock_script() -> packed::Script {
        packed::Script::new_builder()
            .code_hash([8; 32].pack())
            .build()
    }

    fn spend_result(watched_outpoint: packed::OutPoint) -> CkbOutPointSpendTracingResult {
        CkbOutPointSpendTracingResult {
            outpoint: watched_outpoint,
            spending_transaction: TransactionBuilder::default().build(),
            input_index: 2,
            script_group_input_index: 1,
            block_number: 42,
        }
    }

    #[tokio::test]
    async fn historical_committed_result_fires_once_and_removes_outpoint_tracer() {
        let watched_outpoint = outpoint(1);
        let expected = spend_result(watched_outpoint.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let discovered = expected.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let discovered = discovered.clone();
                Box::pin(async move { Ok(Some(discovered)) })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, recv) = oneshot::channel();
        let (registration_send, registration_recv) = oneshot::channel();

        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint,
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");

        timeout(Duration::from_secs(1), registration_recv)
            .await
            .expect("historical registration timed out")
            .expect("historical registration reply dropped")
            .expect("historical registration failed");

        let actual = timeout(Duration::from_secs(1), recv)
            .await
            .expect("historical discovery timed out")
            .expect("callback dropped")
            .expect("historical discovery failed");
        assert_eq!(actual.outpoint, expected.outpoint);
        assert_eq!(actual.spending_transaction, expected.spending_transaction);
        assert_eq!(actual.input_index, expected.input_index);
        assert_eq!(
            actual.script_group_input_index,
            expected.script_group_input_index
        );
        assert_eq!(actual.block_number, expected.block_number);

        actor
            .send_message(CkbOutPointSpendTracingMessage::RunTracers)
            .expect("run tracers after completion");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn none_and_errors_retain_outpoint_tracer_for_a_later_poll() {
        let watched_outpoint = outpoint(2);
        let expected = spend_result(watched_outpoint.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let discovered = expected.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                let call = discovery_calls.fetch_add(1, Ordering::SeqCst);
                let discovered = discovered.clone();
                Box::pin(async move {
                    match call {
                        0 => Ok(None),
                        1 => Err("temporary discovery failure".to_string()),
                        _ => Ok(Some(discovered)),
                    }
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, recv) = oneshot::channel();
        let (registration_send, _registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint,
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");

        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first discovery did not run");
        for expected_calls in 2..=3 {
            timeout(Duration::from_secs(1), async {
                while calls.load(Ordering::SeqCst) < expected_calls {
                    actor
                        .send_message(CkbOutPointSpendTracingMessage::RunTracers)
                        .expect("run retained tracer");
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("retained tracer was not polled again");
        }

        let actual = timeout(Duration::from_secs(1), recv)
            .await
            .expect("retained tracer did not complete")
            .expect("callback dropped")
            .expect("later discovery failed");
        assert_eq!(actual.outpoint, expected.outpoint);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn identical_registrations_receive_the_same_result_once() {
        let watched_outpoint = outpoint(3);
        let expected = spend_result(watched_outpoint.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let gate = Arc::new(Notify::new());
        let discovery_gate = gate.clone();
        let discovered = expected.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let gate = discovery_gate.clone();
                let discovered = discovered.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(Some(discovered))
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (first_send, first_recv) = oneshot::channel();
        let (second_send, second_recv) = oneshot::channel();
        let (first_registration_send, first_registration_recv) = oneshot::channel();
        let (second_registration_send, second_registration_recv) = oneshot::channel();
        for (callback, registration) in [
            (first_send, first_registration_send),
            (second_send, second_registration_send),
        ] {
            actor
                .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                    CkbOutPointSpendTracer {
                        outpoint: watched_outpoint.clone(),
                        lock_script: lock_script(),
                        confirmations: 4,
                        callback: RpcReplyPort::from(callback),
                        registration: RpcReplyPort::from(registration),
                    },
                ))
                .expect("create identical outpoint tracer");
        }

        for registration in [first_registration_recv, second_registration_recv] {
            timeout(Duration::from_secs(1), registration)
                .await
                .expect("identical registration timed out")
                .expect("identical registration reply dropped")
                .expect("identical registration failed");
        }

        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery did not start");
        gate.notify_waiters();

        for recv in [first_recv, second_recv] {
            let actual = timeout(Duration::from_secs(1), recv)
                .await
                .expect("identical tracer timed out")
                .expect("callback dropped")
                .expect("identical tracer failed");
            assert_eq!(actual.outpoint, expected.outpoint);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn conflicting_metadata_errors_immediately_and_preserves_original_tracer() {
        let watched_outpoint = outpoint(4);
        let expected = spend_result(watched_outpoint.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let gate = Arc::new(Notify::new());
        let discovery_gate = gate.clone();
        let discovered = expected.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let gate = discovery_gate.clone();
                let discovered = discovered.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(Some(discovered))
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (original_send, original_recv) = oneshot::channel();
        let (original_registration_send, original_registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint.clone(),
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(original_send),
                    registration: RpcReplyPort::from(original_registration_send),
                },
            ))
            .expect("create original tracer");
        timeout(Duration::from_secs(1), original_registration_recv)
            .await
            .expect("original registration timed out")
            .expect("original registration reply dropped")
            .expect("original registration failed");
        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("original discovery did not start");

        let (conflict_send, conflict_recv) = oneshot::channel();
        let (conflict_registration_send, conflict_registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint,
                    lock_script: conflicting_lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(conflict_send),
                    registration: RpcReplyPort::from(conflict_registration_send),
                },
            ))
            .expect("create conflicting tracer");
        let conflict = timeout(Duration::from_millis(100), conflict_registration_recv)
            .await
            .expect("conflict was not reported immediately")
            .expect("conflict registration reply dropped");
        assert!(conflict.is_err());

        assert!(
            timeout(Duration::from_millis(25), conflict_recv)
                .await
                .expect("conflicting callback sender remained open")
                .is_err(),
            "registration conflict was reported through the spend callback"
        );

        gate.notify_waiters();
        let original = timeout(Duration::from_secs(1), original_recv)
            .await
            .expect("original tracer timed out")
            .expect("original callback dropped")
            .expect("original tracer failed");
        assert_eq!(original.outpoint, expected.outpoint);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn remove_tracers_prevents_an_in_flight_result_from_firing_callback() {
        let watched_outpoint = outpoint(5);
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let gate = Arc::new(Notify::new());
        let discovery_gate = gate.clone();
        let discovered = spend_result(watched_outpoint.clone());
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let gate = discovery_gate.clone();
                let discovered = discovered.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(Some(discovered))
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, mut recv) = oneshot::channel();
        let (registration_send, _registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint.clone(),
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");
        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery did not start");

        actor
            .send_message(CkbOutPointSpendTracingMessage::RemoveTracers(
                watched_outpoint,
            ))
            .expect("remove tracers");
        gate.notify_waiters();
        let callback = timeout(Duration::from_millis(100), &mut recv)
            .await
            .expect("removed callback remained open");
        assert!(callback.is_err(), "removed callback received a result");

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn remove_tracers_cancels_in_flight_discovery() {
        let watched_outpoint = outpoint(7);
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let (drop_send, drop_recv) = oneshot::channel();
        let drop_send = Arc::new(Mutex::new(Some(drop_send)));
        let discovery_drop_send = drop_send.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let drop_send = discovery_drop_send
                    .lock()
                    .expect("lock drop sender")
                    .take()
                    .expect("discovery only runs once");
                Box::pin(async move {
                    struct DropSignal(Option<oneshot::Sender<()>>);

                    impl Drop for DropSignal {
                        fn drop(&mut self) {
                            let _ = self.0.take().expect("drop signal exists").send(());
                        }
                    }

                    let _drop_signal = DropSignal(Some(drop_send));
                    pending().await
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, _recv) = oneshot::channel();
        let (registration_send, _registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint.clone(),
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");
        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery did not start");

        actor
            .send_message(CkbOutPointSpendTracingMessage::RemoveTracers(
                watched_outpoint,
            ))
            .expect("remove tracers");
        timeout(Duration::from_millis(100), drop_recv)
            .await
            .expect("in-flight discovery was not cancelled")
            .expect("drop signal sender disappeared");

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn repeated_run_tracers_does_not_overlap_or_duplicate_callbacks() {
        let watched_outpoint = outpoint(6);
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let gate = Arc::new(Notify::new());
        let discovery_gate = gate.clone();
        let discovered = spend_result(watched_outpoint.clone());
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                discovery_calls.fetch_add(1, Ordering::SeqCst);
                let gate = discovery_gate.clone();
                let discovered = discovered.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(Some(discovered))
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, recv) = oneshot::channel();
        let (registration_send, _registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint,
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");
        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery did not start");

        for _ in 0..3 {
            actor
                .send_message(CkbOutPointSpendTracingMessage::RunTracers)
                .expect("run tracers");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        gate.notify_waiters();
        timeout(Duration::from_secs(1), recv)
            .await
            .expect("tracer timed out")
            .expect("callback dropped")
            .expect("tracer failed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }

    #[tokio::test]
    async fn panicking_discovery_clears_task_handle_and_retries_on_next_run() {
        let watched_outpoint = outpoint(8);
        let expected = spend_result(watched_outpoint.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls = calls.clone();
        let discovered = expected.clone();
        let actor_impl = CkbOutPointSpendTracingActor::with_discovery(
            move |_rpc_url, _lock_script, _outpoint, _confirmations| {
                let call = discovery_calls.fetch_add(1, Ordering::SeqCst);
                let discovered = discovered.clone();
                Box::pin(async move {
                    if call == 0 {
                        panic!("injected discovery panic");
                    }
                    Ok(Some(discovered))
                })
            },
        );
        let (actor, handle) = Actor::spawn(
            None,
            actor_impl,
            CkbOutPointSpendTracingArguments {
                rpc_url: "unused".to_string(),
                polling_interval: Duration::from_secs(3600),
            },
        )
        .await
        .expect("spawn outpoint tracing actor");
        let (send, recv) = oneshot::channel();
        let (registration_send, _registration_recv) = oneshot::channel();
        actor
            .send_message(CkbOutPointSpendTracingMessage::CreateTracer(
                CkbOutPointSpendTracer {
                    outpoint: watched_outpoint,
                    lock_script: lock_script(),
                    confirmations: 4,
                    callback: RpcReplyPort::from(send),
                    registration: RpcReplyPort::from(registration_send),
                },
            ))
            .expect("create outpoint tracer");

        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicking discovery did not run");

        // A panic must clear the task handle so the next run retries instead of
        // stalling forever. Keep nudging RunTracers until a retry happens.
        timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 2 {
                actor
                    .send_message(CkbOutPointSpendTracingMessage::RunTracers)
                    .expect("run tracers");
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicking discovery stalled: task handle was not cleared");

        let actual = timeout(Duration::from_secs(1), recv)
            .await
            .expect("retried tracer timed out")
            .expect("callback dropped")
            .expect("retried tracer failed");
        assert_eq!(actual.outpoint, expected.outpoint);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        actor.stop(None);
        handle.await.expect("stop outpoint tracing actor");
    }
}
