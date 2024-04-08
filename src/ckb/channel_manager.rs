use tentacle::secio::PeerId;
use tokio::sync::mpsc;

use super::{
    types::{OpenChannel, PCNMessage},
    Command as CCommand,
};

pub enum ChannelCommand {
    OpenChannel(OpenChannel),
}

pub struct ChannelManager {
    command_sender: mpsc::Sender<CCommand>,
    receiver: mpsc::Receiver<ChannelCommand>,
}

impl ChannelManager {
    pub fn new(
        command_sender: mpsc::Sender<CCommand>,
        chnanel_command_receiver: mpsc::Receiver<ChannelCommand>,
    ) -> Self {
        Self {
            command_sender,
            receiver: chnanel_command_receiver,
        }
    }

    pub async fn send_command(&self, command: CCommand) {
        self.command_sender
            .send(command)
            .await
            .expect("send command failed");
    }
}

impl ChannelManager {
    fn handle_peer_message(&self, peer: PeerId, message: PCNMessage) {}

    fn handle_user_command(&self, command: ChannelCommand) {}
}
