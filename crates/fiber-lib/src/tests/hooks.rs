//! Payloads for the `#[cfg(test)] Test(..)` variants of the actor message enums.
//!
//! They live here instead of next to their actors so those modules can keep a
//! private `mod tests;`: only the payload has to be nameable crate-wide, the
//! handlers that touch actor state stay in `<actor>/tests.rs`.

use ractor::{ActorRef, RpcReplyPort};
use tokio::sync::oneshot;

#[cfg(not(target_arch = "wasm32"))]
use fiber_types::CchOrder;
use fiber_types::Hash256;

#[cfg(not(target_arch = "wasm32"))]
use crate::cch::CchError;
use crate::fiber::channel::ChannelActorMessage;
use crate::fiber::network::{FiberMessageWithTarget, TestFiberMessageHold};
use crate::fiber::payment::PaymentActorMessage;
use crate::fiber_types::PaymentSessionKey;

#[derive(Clone, Debug)]
pub struct TestPendingMessages {
    pub pending_peer_message_count: usize,
    pub has_pending_peer_commitment: bool,
}
/// Test-only commands accepted through `FiberActorCommand::Test`.
#[derive(Debug)]
pub enum TestCommand {
    InstallChannelActor(Hash256, ActorRef<ChannelActorMessage>, RpcReplyPort<()>),
    SetMessageHold(TestFiberMessageHold, RpcReplyPort<()>),
    TakeHeldMessages(RpcReplyPort<Vec<FiberMessageWithTarget>>),
    ReleaseHeldMessages(RpcReplyPort<Result<(), String>>),
    HeldMessageCount(RpcReplyPort<usize>),
    SetTrampolineSettlementPaused(bool, RpcReplyPort<()>),
    GetPaymentActor(
        PaymentSessionKey,
        RpcReplyPort<Option<ActorRef<PaymentActorMessage>>>,
    ),
    SetPaymentActor(
        PaymentSessionKey,
        ActorRef<PaymentActorMessage>,
        RpcReplyPort<()>,
    ),
}
/// Test-only channel actor messages accepted through `ChannelActorMessage::Test`.
#[derive(Debug)]
pub enum TestChannelMessage {
    GetPendingMessages(RpcReplyPort<TestPendingMessages>),
}
/// Test-only channel commands accepted through `ChannelCommand::Test`.
#[derive(Debug)]
pub enum TestChannelCommand {
    /// Pause local processing until the test releases the returned sender.
    Pause(RpcReplyPort<oneshot::Sender<()>>),
}
/// Test-only CCH messages accepted through `CchMessage::Test`.
#[cfg(not(target_arch = "wasm32"))]
pub enum TestMessage {
    InsertOrder(CchOrder, RpcReplyPort<Result<(), CchError>>),
    GetStartupRecoveryStatus(RpcReplyPort<bool>),
}
