use tokio::sync::oneshot;

use super::*;

struct SlowStoppingActor;

#[async_trait::async_trait]
impl Actor for SlowStoppingActor {
    type Msg = ();
    type State = (Option<oneshot::Sender<()>>, Option<oneshot::Receiver<()>>);
    type Arguments = Self::State;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.0.take().unwrap().send(()).unwrap();
        state.1.take().unwrap().await.unwrap();
        Ok(())
    }
}

#[tokio::test]
async fn root_shutdown_waits_for_child_post_stop() {
    let root = RootActor::start_for_test(TaskTracker::new(), CancellationToken::new()).await;
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (_child, _handle) = Actor::spawn_linked(
        None,
        SlowStoppingActor,
        (Some(started_tx), Some(release_rx)),
        root.get_cell(),
    )
    .await
    .unwrap();
    let shutdown = tokio::spawn(async move { root.stop_and_wait(None, None).await });

    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!shutdown.is_finished());
    release_tx.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
}

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
    let root = RootActor::start_for_test(TaskTracker::new(), CancellationToken::new()).await;
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
