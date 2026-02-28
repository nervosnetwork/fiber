use fiber_types::CchOrder;
use ractor::ActorRef;

use crate::cch::{actions::ActionExecutor, actor::CchState, CchMessage, CchOrderStore};

pub struct TrackOutgoingPaymentDispatcher;

impl TrackOutgoingPaymentDispatcher {
    pub fn dispatch<S: CchOrderStore>(
        _state: &CchState<S>,
        _cch_actor_ref: &ActorRef<CchMessage>,
        _order: &CchOrder,
        _retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        // `CchActor` will track all fiber payments, so there's nothing to do here to track a single payment.
        None
    }
}
