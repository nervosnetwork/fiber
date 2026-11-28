use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{debug, error};

pub(crate) fn log_actor_failed(who: impl std::fmt::Debug, err: impl std::fmt::Debug) {
    error!("Actor unexpectedly panicked (id: {:?}): {:?}", who, err);
}

/// A root actor that listens for cancellation token and stops all sub actors (those who started by spawn_linked).
pub struct RootActor;

pub type RootActorMessage = String;

#[cfg(test)]
static NEXT_TEST_ROOT_ACTOR_ID: AtomicU64 = AtomicU64::new(0);

impl RootActor {
    pub async fn start(
        tracker: TaskTracker,
        token: CancellationToken,
    ) -> ActorRef<RootActorMessage> {
        Self::start_named(Some("root actor".to_string()), tracker, token).await
    }

    async fn start_named(
        name: Option<String>,
        tracker: TaskTracker,
        token: CancellationToken,
    ) -> ActorRef<RootActorMessage> {
        Actor::spawn(name, RootActor {}, (tracker, token))
            .await
            .expect("start root actor")
            .0
    }

    #[cfg(test)]
    pub(crate) async fn start_for_test(
        tracker: TaskTracker,
        token: CancellationToken,
    ) -> ActorRef<RootActorMessage> {
        let id = NEXT_TEST_ROOT_ACTOR_ID.fetch_add(1, Ordering::Relaxed);
        Self::start_named(Some(format!("test root actor {id}")), tracker, token).await
    }
}

#[async_trait::async_trait]
impl Actor for RootActor {
    type Msg = RootActorMessage;
    type State = ();
    type Arguments = (TaskTracker, CancellationToken);

    /// Spawn a thread that waits for token to be cancelled,
    /// after that kill all sub actors.
    #[allow(unused)]
    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (tracker, token): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        #[cfg(not(target_arch = "wasm32"))]
        tracker.spawn(async move {
            token.cancelled().await;
            debug!("Shutting down root actor due to cancellation token");
            myself.stop(Some("Cancellation token received".to_owned()));
        });
        Ok(())
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Root actor stopped");
        myself
            .get_cell()
            .stop_children_and_wait(Some("Root actor stopped".to_string()), None)
            .await;
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _state, reason) => match reason {
                Some(reason) => {
                    debug!("Actor terminated for {:?} (id: {:?})", reason, who,);
                }
                None => {
                    debug!("Actor terminated for unknown reason (id: {:?})", who);
                }
            },
            SupervisionEvent::ActorFailed(who, err) => {
                log_actor_failed(who, err);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
