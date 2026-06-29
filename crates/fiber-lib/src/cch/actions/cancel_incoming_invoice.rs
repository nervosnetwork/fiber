use anyhow::{anyhow, Result};
use lnd_grpc_tonic_client::invoicesrpc;
use ractor::ActorRef;

use crate::cch::{
    actions::{
        backend_dispatchers::{dispatch_invoice_handler, InvoiceHandlerType},
        ActionExecutor,
    },
    actor::CchState,
    trackers::LndConnectionInfo,
    CchFiberAgentRef, CchFiberCancelInvoiceError, CchMessage, CchOrderStore,
};
use fiber_types::{CchOrder, CchOrderStatus, Hash256};

pub struct CancelIncomingInvoiceDispatcher;

pub struct CancelFiberIncomingInvoiceExecutor {
    payment_hash: Hash256,
    fiber_agent_ref: CchFiberAgentRef,
}

#[async_trait::async_trait]
impl ActionExecutor for CancelFiberIncomingInvoiceExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        match self
            .fiber_agent_ref
            .call_cancel_invoice(self.payment_hash)
            .await
        {
            Ok(()) => Ok(()),
            Err(CchFiberCancelInvoiceError::Permanent(err)) => {
                tracing::warn!(
                    "CancelFiberIncomingInvoiceExecutor permanent failure for payment_hash={:x}: {}",
                    self.payment_hash,
                    err
                );
                Ok(())
            }
            Err(CchFiberCancelInvoiceError::Transient(err)) => {
                Err(anyhow!("CancelFiberIncomingInvoiceExecutor error: {}", err))
            }
        }
    }
}

pub struct CancelLightningIncomingInvoiceExecutor {
    payment_hash: Hash256,
    lnd_connection: LndConnectionInfo,
}

#[async_trait::async_trait]
impl ActionExecutor for CancelLightningIncomingInvoiceExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let req = invoicesrpc::CancelInvoiceMsg {
            payment_hash: self.payment_hash.as_ref().to_vec(),
        };

        let mut client = self.lnd_connection.create_invoices_client().await?;
        match client.cancel_invoice(req).await {
            Ok(_) => Ok(()),
            Err(err) if Self::is_permanent_error(&err) => {
                if err.code() == tonic::Code::FailedPrecondition {
                    tracing::warn!(
                        "CancelLightningIncomingInvoiceExecutor received FailedPrecondition for payment_hash={:x}: {}",
                        self.payment_hash,
                        err.message()
                    );
                }
                Ok(())
            }
            Err(err) => Err(anyhow!(
                "CancelLightningIncomingInvoiceExecutor error: {:?}",
                err
            )),
        }
    }
}

impl CancelLightningIncomingInvoiceExecutor {
    fn is_permanent_error(status: &tonic::Status) -> bool {
        matches!(
            status.code(),
            tonic::Code::InvalidArgument | tonic::Code::NotFound | tonic::Code::FailedPrecondition
        )
    }
}

impl CancelIncomingInvoiceDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::Failed && order.payment_preimage.is_none()
    }

    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        _cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        _retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }

        match dispatch_invoice_handler(order) {
            InvoiceHandlerType::Fiber => Some(Box::new(CancelFiberIncomingInvoiceExecutor {
                payment_hash: order.payment_hash,
                fiber_agent_ref: state.fiber_agent_ref.clone(),
            })),
            InvoiceHandlerType::Lightning => {
                Some(Box::new(CancelLightningIncomingInvoiceExecutor {
                    payment_hash: order.payment_hash,
                    lnd_connection: state.lnd_connection.clone(),
                }))
            }
        }
    }
}
