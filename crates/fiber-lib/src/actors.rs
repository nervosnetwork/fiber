use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{debug, error};

pub(crate) fn log_actor_failed(who: impl std::fmt::Debug, err: impl std::fmt::Debug) {
    error!("Actor unexpectedly panicked (id: {:?}): {:?}", who, err);
}

/// A root actor that listens for cancellation token and stops all sub actors (those who started by spawn_linked).
pub struct RootActor;

pub type RootActorMessage = String;

impl RootActor {
    pub async fn start(
        tracker: TaskTracker,
        token: CancellationToken,
    ) -> ActorRef<RootActorMessage> {
        Actor::spawn(
            Some("root actor".to_string()),
            RootActor {},
            (tracker, token),
        )
        .await
        .expect("start root actor")
        .0
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
mod tests {
    use super::*;

    struct PanickingActor;

    #[async_trait::async_trait]
    impl Actor for PanickingActor {
        type Msg = ();
        type State = ();
        type Arguments = ();

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            _args: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            _message: Self::Msg,
            _state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            panic!("child actor panic for supervision test");
        }
    }

    #[tokio::test]
    async fn root_actor_does_not_panic_when_linked_child_fails() {
        let root = RootActor::start(TaskTracker::new(), CancellationToken::new()).await;
        let (child, _) = Actor::spawn_linked(
            Some("panicking child".to_string()),
            PanickingActor,
            (),
            root.get_cell(),
        )
        .await
        .expect("start panicking actor");

        child
            .send_message(())
            .expect("child receives panic trigger");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        root.send_message("root should still accept messages".to_string())
            .expect("root actor should remain alive after linked child failure");
    }
}
