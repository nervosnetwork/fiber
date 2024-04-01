use tokio::{select, sync::mpsc};
use tokio_util::sync::CancellationToken;

use super::CchCommand;

pub(super) struct CchState {
    pub token: CancellationToken,
    pub command_receiver: mpsc::Receiver<CchCommand>,
}

pub(super) struct CchService {
    state: CchState,
}

impl CchService {
    pub fn new(state: CchState) -> Self {
        Self { state }
    }

    pub async fn run(mut self) {
        loop {
            select! {
                _ = self.state.token.cancelled() => {
                    log::debug!("Cancellation received, shutting down cch service");
                    break;
                }
                command = self.state.command_receiver.recv() => {
                    match command {
                        None => {
                            log::debug!("Command receiver completed, shutting down tentacle service");
                            break;
                        }
                        Some(command) => {
                            self.process_command(command).await;
                        }
                    }
                }
            }
        }
    }

    async fn process_command(&self, command: CchCommand) {
        log::debug!("CchCommand received: {:?}", command);
    }
}
