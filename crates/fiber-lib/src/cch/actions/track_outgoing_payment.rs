use ractor::ActorRef;

use crate::cch::{actions::ActionExecutor, actor::CchState, CchMessage, CchOrder};

pub struct TrackOutgoingPaymentDispatcher;

impl TrackOutgoingPaymentDispatcher {
    pub fn dispatch(
        _state: &mut CchState,
        _cch_actor_ref: &ActorRef<CchMessage>,
        _order: &CchOrder,
    ) -> Option<Box<dyn ActionExecutor>> {
        // `CchActor` will track all fiber payments, so there's nothing to do here to track a single payment.
        None
    }
}
