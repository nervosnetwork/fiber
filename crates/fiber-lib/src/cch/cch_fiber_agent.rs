use anyhow::{anyhow, Result};
use jsonrpsee::{
    core::client::ClientT as _,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use ractor::{call, ActorRef};
use std::str::FromStr as _;

use crate::{
    fiber::{
        network::SendPaymentCommand, payment::PaymentStatus, types::Hash256, NetworkActorCommand,
        NetworkActorMessage,
    },
    invoice::CkbInvoice,
    rpc::{
        invoice::{InvoiceResult, NewInvoiceParams, SettleInvoiceParams, SettleInvoiceResult},
        payment::{GetPaymentCommandResult, SendPaymentCommandParams},
    },
};

pub enum CchFiberAgent {
    InProcess {
        network_actor: ActorRef<NetworkActorMessage>,
    },
    Http {
        client: HttpClient,
    },
}

impl CchFiberAgent {
    pub fn try_new(
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        fiber_rpc_url: Option<&str>,
    ) -> Result<Self> {
        match (network_actor, fiber_rpc_url) {
            (Some(network_actor), _) => Ok(CchFiberAgent::InProcess { network_actor }),
            (None, Some(url)) => {
                let client = HttpClientBuilder::default().build(url)?;
                Ok(CchFiberAgent::Http { client })
            }
            (None, None) => Err(anyhow!("require either network actor or Fiber RPC URL")),
        }
    }

    /// Add an invoice.
    ///
    /// Returns the final invoice. The returned invoice may include extra attributes such as payee
    /// pubkey and signature.
    pub async fn add_invoice(&self, invoice: CkbInvoice) -> Result<CkbInvoice> {
        match self {
            Self::InProcess { network_actor } => {
                // For in process agent, the invoice should have already been signed
                let invoice_cloned = invoice.clone();
                let message = move |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                        invoice.clone(),
                        None,
                        rpc_reply,
                    ))
                };

                call!(network_actor, message).expect("call actor")?;
                Ok(invoice_cloned)
            }
            Self::Http { client } => {
                let invoice_params = NewInvoiceParams {
                    amount: invoice
                        .amount
                        .ok_or_else(|| anyhow!("cch invoice requires amount"))?,
                    payment_hash: Some(*invoice.payment_hash()),
                    hash_algorithm: invoice.hash_algorithm().copied(),
                    expiry: invoice.expiry_time().map(|duration| duration.as_secs()),
                    final_expiry_delta: invoice.final_tlc_minimum_expiry_delta().copied(),
                    udt_type_script: invoice
                        .udt_type_script()
                        .map(|script| script.clone().into()),
                    description: invoice.description().cloned(),
                    currency: invoice.currency,
                    payment_preimage: None,
                    fallback_address: invoice.fallback_address().cloned(),
                    allow_mpp: Some(invoice.allow_mpp()),
                };
                let response = client
                    .request::<InvoiceResult, _>("new_invoice", rpc_params![invoice_params])
                    .await?;
                // Return the signed invoice from the response
                CkbInvoice::from_str(&response.invoice_address).map_err(Into::into)
            }
        }
    }

    pub async fn send_payment(&self, pay_req: String) -> Result<PaymentStatus> {
        match self {
            Self::InProcess { network_actor } => {
                let message = |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                        SendPaymentCommand {
                            invoice: Some(pay_req),
                            ..Default::default()
                        },
                        rpc_reply,
                    ))
                };

                Ok(call!(network_actor, message)
                    .expect("call actor")
                    .map_err(|err| anyhow!("{}", err))?
                    .status)
            }
            Self::Http { client } => {
                let payment_params = SendPaymentCommandParams {
                    invoice: Some(pay_req),
                    ..Default::default()
                };
                let response = client
                    .request::<GetPaymentCommandResult, _>(
                        "send_payment",
                        rpc_params![payment_params],
                    )
                    .await?;
                Ok(response.status)
            }
        }
    }

    pub async fn settle_invoice(
        &self,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<()> {
        match self {
            Self::InProcess { network_actor } => {
                let command = move |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                        payment_hash,
                        payment_preimage,
                        rpc_reply,
                    ))
                };
                call!(network_actor, command).expect("call actor")?;
                Ok(())
            }
            Self::Http { client } => {
                let settle_invoice_params = SettleInvoiceParams {
                    payment_hash,
                    payment_preimage,
                };
                client
                    .request::<SettleInvoiceResult, _>(
                        "settle_invoice",
                        rpc_params![settle_invoice_params],
                    )
                    .await?;
                Ok(())
            }
        }
    }
}
