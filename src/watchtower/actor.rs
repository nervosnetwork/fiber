use ractor::{Actor, ActorProcessingErr, ActorRef};
use tracing::debug;

pub struct WatchtowerActor {}

pub enum WatchtowerMessage {
    ClientRequest,
    PeriodicCheck,
}

pub struct WatchtowerState {}

#[ractor::async_trait]
impl Actor for WatchtowerActor {
    type Msg = WatchtowerMessage;
    type State = WatchtowerState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        config: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State {})
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            WatchtowerMessage::ClientRequest => {
                // handle client request
            }
            WatchtowerMessage::PeriodicCheck => {
                debug!("Periodic check");
            }
        }
        Ok(())
    }
}
