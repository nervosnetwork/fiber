use super::{InvoiceCommand, InvoiceStore, NewInvoiceParams};
use crate::{invoice::*, rpc::InvoiceCommandWithReply};
use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use tokio::{select, sync::mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub async fn start_invoice<S: InvoiceStore + Send + 'static>(
    command_receiver: mpsc::Receiver<InvoiceCommandWithReply>,
    token: CancellationToken,
    tracker: TaskTracker,
    store: S,
) {
    let service = InvoiceService {
        command_receiver,
        token,
        store,
    };
    tracker.spawn(async move {
        service.run().await;
    });
}
struct InvoiceService<S> {
    token: CancellationToken,
    command_receiver: mpsc::Receiver<InvoiceCommandWithReply>,
    store: S,
}

impl<S> InvoiceService<S>
where
    S: InvoiceStore,
{
    pub async fn run(mut self) {
        loop {
            select! {
                _ = self.token.cancelled() => {
                    log::debug!("Cancellation received, shutting down cch service");
                    break;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        None => {
                            log::debug!("Command receiver completed, shutting down tentacle service");
                            break;
                        }
                        Some(command) => {
                            let command_name = command.0.name();
                            log::info!("Process cch command {}", command_name);

                            match self.process_command(command.0, command.1).await {
                                Ok(_) => {}
                                Err(err) => {
                                    log::error!("Error processing command {}: {:?}", command_name, err);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn process_command(
        &mut self,
        command: InvoiceCommand,
        response: Option<mpsc::Sender<crate::Result<String>>>,
    ) -> Result<(), anyhow::Error> {
        log::debug!("InvoiceCommand received: {:?}", command);
        let res = match command {
            InvoiceCommand::NewInvoice(params) => self.new_invoice(params).await,
            InvoiceCommand::ParseInvoice(params) => self.parse_invoice(params).await,
        };
        let response = response.expect("response channel");
        match res {
            Ok(invoice) => {
                let data = json!({
                    "invoice": json!(invoice),
                    "encode_payment": invoice.to_string(),
                    "payment_hash": invoice.payment_hash(),
                })
                .to_string();
                let _ = response.send(Ok(data)).await;
            }
            Err(err) => {
                let _ = response.send(Err(err.into())).await;
            }
        }
        Ok(())
    }

    async fn new_invoice(
        &mut self,
        new_invoice: NewInvoiceParams,
    ) -> Result<CkbInvoice, InvoiceError> {
        let mut invoice_builder =
            InvoiceBuilder::new(new_invoice.currency).amount(Some(new_invoice.amount));
        if let Some(description) = new_invoice.description {
            invoice_builder = invoice_builder.description(description);
        };
        if let Some(payment_hash) = new_invoice.payment_hash {
            invoice_builder = invoice_builder.payment_hash(payment_hash);
        };
        if let Some(payment_preimage) = new_invoice.payment_preimage {
            invoice_builder = invoice_builder.payment_preimage(payment_preimage);
        };
        if let Some(expiry) = new_invoice.expiry {
            let duration: Duration = Duration::from_secs(expiry);
            invoice_builder = invoice_builder.expiry_time(duration);
        };
        if let Some(fallback_address) = new_invoice.fallback_address {
            invoice_builder = invoice_builder.fallback_address(fallback_address);
        };
        if let Some(final_cltv) = new_invoice.final_cltv {
            invoice_builder = invoice_builder.final_cltv(final_cltv);
        };

        let invoice = invoice_builder.build();
        if let Ok(invoice) = &invoice {
            self.store.insert_invoice(invoice.clone());
        }
        invoice
    }

    async fn parse_invoice(&mut self, invoice: String) -> Result<CkbInvoice, InvoiceError> {
        invoice.parse::<CkbInvoice>()
    }
}
