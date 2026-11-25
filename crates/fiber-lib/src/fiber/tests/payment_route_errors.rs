use ractor::{Actor, ActorProcessingErr, ActorRef};

use super::*;
use crate::tests::test_utils::NetworkNode;

struct RetryMailbox;

#[async_trait::async_trait]
impl Actor for RetryMailbox {
    type Msg = PaymentActorMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Msg,
        _: &mut (),
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

#[tokio::test]
async fn trampoline_mpp_capacity_probe_preserves_no_path_as_temporary() {
    let mut node = NetworkNode::new_with_node_name("trampoline-mpp-no-path").await;
    let target = Privkey::from(&[2; 32]).pubkey();
    let payment_hash = crate::gen_rand_sha256_hash();
    let request = SendPaymentDataBuilder::new(target, 1_000, payment_hash)
        .max_fee_amount(Some(100))
        .max_parts(Some(2))
        .allow_mpp(true)
        .trampoline_context(Some(TrampolineContext::default()))
        .build()
        .unwrap();
    let session_key = request.session_key();
    node.store
        .insert_payment_session(PaymentSession::new_session(&node.store, request, 2));

    let actor = PaymentActor::new(
        node.store.clone(),
        node.network_graph.clone(),
        FiberActorRef::from_network(&node.network_actor),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key,
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (mailbox, mailbox_task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let result = actor
        .resume_payment_session(mailbox.clone(), &mut state, None)
        .await;
    assert!(matches!(
        result,
        Err(Error::NetworkGraphError(PathFindError::NoPathFound))
    ));
    let failed = node.store.get_payment_session(session_key).unwrap();
    assert_eq!(failed.status, PaymentStatus::Failed);
    assert_eq!(
        failed.last_error_code,
        Some(TlcErrorCode::TemporaryNodeFailure)
    );

    mailbox.stop(None);
    mailbox_task.await.unwrap();
    node.stop().await;
}

#[tokio::test]
async fn trampoline_route_configuration_error_clears_stale_temporary_code() {
    let mut node = NetworkNode::new_with_node_name("trampoline-invalid-route").await;
    let target = Privkey::from(&[3; 32]).pubkey();
    let payment_hash = crate::gen_rand_sha256_hash();
    let mut request = SendPaymentDataBuilder::new(target, 1_000, payment_hash)
        .max_fee_amount(Some(100))
        .max_parts(Some(2))
        .allow_mpp(true)
        .trampoline_context(Some(TrampolineContext::default()))
        .build()
        .unwrap();
    // Simulate a persisted session whose prior failed attempt was temporary,
    // but whose current routing configuration is now invalid.
    request.tlc_expiry_limit = 0;
    let session_key = request.session_key();
    let mut session = PaymentSession::new_session(&node.store, request, 2);
    session.last_error_code = Some(TlcErrorCode::TemporaryNodeFailure);
    node.store.insert_payment_session(session);

    let actor = PaymentActor::new(
        node.store.clone(),
        node.network_graph.clone(),
        FiberActorRef::from_network(&node.network_actor),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key,
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (mailbox, mailbox_task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let result = actor
        .resume_payment_session(mailbox.clone(), &mut state, None)
        .await;
    assert!(matches!(
        result,
        Err(Error::NetworkGraphError(PathFindError::Overflow(_)))
    ));
    let failed = node.store.get_payment_session(session_key).unwrap();
    assert_eq!(failed.status, PaymentStatus::Failed);
    assert_eq!(failed.last_error_code, None);

    mailbox.stop(None);
    mailbox_task.await.unwrap();
    node.stop().await;
}
