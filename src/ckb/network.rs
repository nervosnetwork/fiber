use futures::{
    channel::oneshot::{channel, Sender},
    future::select,
    prelude::*,
};
use log::{debug, info};
use std::collections::HashMap;
use std::{str, time::Duration};
use tentacle::{bytes::Bytes, secio::PeerId};
use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    service::{
        ProtocolHandle, ProtocolMeta, Service, ServiceError, ServiceEvent, TargetProtocol,
        TargetSession,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::CkbConfig;

// Any protocol will be abstracted into a ProtocolMeta structure.
// From an implementation point of view, tentacle treats any protocol equally
fn create_meta(id: ProtocolId) -> ProtocolMeta {
    MetaBuilder::new()
        .id(id)
        .service_handle(move || {
            // All protocol use the same handle.
            // This is just an example. In the actual environment, this should be a different handle.
            let handle = Box::new(PHandle {
                count: 0,
                connected_session_ids: Vec::new(),
                clear_handle: HashMap::new(),
            });
            ProtocolHandle::Callback(handle)
        })
        .build()
}

#[derive(Default)]
struct PHandle {
    count: usize,
    connected_session_ids: Vec<SessionId>,
    clear_handle: HashMap<SessionId, Sender<()>>,
}

#[async_trait]
impl ServiceProtocol for PHandle {
    async fn init(&mut self, context: &mut ProtocolContext) {
        if context.proto_id == 0.into() {
            let _ = context
                .set_service_notify(0.into(), Duration::from_secs(5), 3)
                .await;
        }
    }

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        self.connected_session_ids.push(session.id);
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );
        info!("connected sessions are: {:?}", self.connected_session_ids);

        if context.proto_id != 1.into() {
            return;
        }

        // Register a scheduled task to send data to the remote peer.
        // Clear the task via channel when disconnected
        let (sender, receiver) = channel();
        self.clear_handle.insert(session.id, sender);
        let session_id = session.id;
        let interval_sender = context.control().clone();

        let interval_send_task = async move {
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now(), Duration::from_secs(5));
            loop {
                interval.tick().await;
                let _ = interval_sender
                    .send_message_to(session_id, 1.into(), Bytes::from("I am a interval message"))
                    .await;
            }
        };

        let task = select(receiver, interval_send_task.boxed());

        let _ = context
            .future_task(async move {
                task.await;
            })
            .await;
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        let new_list = self
            .connected_session_ids
            .iter()
            .filter(|&id| id != &context.session.id)
            .cloned()
            .collect();
        self.connected_session_ids = new_list;

        if let Some(handle) = self.clear_handle.remove(&context.session.id) {
            let _ = handle.send(());
        }

        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        self.count += 1;
        info!(
            "received from [{}]: proto [{}] data {:?}, message count: {}",
            context.session.id,
            context.proto_id,
            str::from_utf8(data.as_ref()).unwrap(),
            self.count
        );
    }

    async fn notify(&mut self, context: &mut ProtocolContext, token: u64) {
        info!(
            "proto [{}] received notify token: {}",
            context.proto_id, token
        );
    }
}

pub struct SHandle;

#[async_trait]
impl ServiceHandle for SHandle {
    // A lot of internal error events will be output here, but not all errors need to close the service,
    // some just tell users that they need to pay attention
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        info!("service error: {:?}", error);
    }
    async fn handle_event(&mut self, context: &mut ServiceContext, event: ServiceEvent) {
        info!("service event: {:?}", event);
        if let ServiceEvent::SessionOpen { .. } = event {
            let delay_sender = context.control().clone();

            let _ = context
                .future_task(async move {
                    tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_secs(3))
                        .await;
                    let _ = delay_sender
                        .filter_broadcast(
                            TargetSession::All,
                            0.into(),
                            Bytes::from("I am a delayed message"),
                        )
                        .await;
                })
                .await;
        }
    }
}

pub async fn start_ckb(config: CkbConfig, token: CancellationToken, tracker: TaskTracker) {
    let kp = config
        .read_or_generate_secret_key()
        .expect("read or generate secret key");
    let pk = kp.public_key();
    let mut service = ServiceBuilder::default()
        .insert_protocol(create_meta(0.into()))
        .insert_protocol(create_meta(1.into()))
        .key_pair(kp)
        .build(SHandle);
    let listen_addr = service
        .listen(
            format!("/ip4/127.0.0.1/tcp/{}", config.listening_port)
                .parse()
                .expect("valid tentacle address"),
        )
        .await
        .expect("listen tentacle");

    info!(
        "Started listening tentacle on {}/p2p/{}",
        listen_addr,
        PeerId::from(pk).to_base58()
    );

    let controller = service.control().to_owned();

    tracker.spawn(async move {
        service.run().await;
        debug!("Tentacle service shutdown");
    });

    tracker.spawn(async move {
        let _ = token.cancelled().await;
        debug!("Cancellation received, shutting down tentacle service");
        let _ = controller.shutdown().await;
    });
}
