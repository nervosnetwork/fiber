use super::{InvoiceCommand, InvoicesDb, NewInvoiceParams};
use crate::{invoice::*, rpc::InvoiceCommandWithReply};
use anyhow::Result;
use service::{invoice::Currency, utils::vec_to_u8_32};
use std::{str::FromStr, time::Duration};
use tokio::{select, sync::mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub async fn start_invoice(
    command_receiver: mpsc::Receiver<InvoiceCommandWithReply>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let service = InvoiceService {
        command_receiver,
        token,
        invoices_db: Default::default(),
    };
    tracker.spawn(async move {
        service.run().await;
    });
}
struct InvoiceService {
    token: CancellationToken,
    command_receiver: mpsc::Receiver<InvoiceCommandWithReply>,
    invoices_db: InvoicesDb,
}

impl InvoiceService {
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
        response: Option<mpsc::Sender<crate::Result<CkbInvoice>>>,
    ) -> Result<(), anyhow::Error> {
        log::debug!("InvoiceCommand received: {:?}", command);
        match command {
            InvoiceCommand::NewInvoice(params) => {
                let res = self.new_invoice(params).await;
                let response = response.expect("response channel");
                match res {
                    Ok(invoice) => {
                        let _ = response.send(Ok(invoice)).await;
                    }
                    Err(err) => {
                        let _ = response.send(Err(err.into())).await;
                    }
                }
                Ok(())
            }
            InvoiceCommand::ParseInvoice(params) => self.parse_invoice(params, response).await,
        }
    }

    async fn new_invoice(
        &mut self,
        new_invoice: NewInvoiceParams,
    ) -> Result<CkbInvoice, InvoiceError> {
        eprintln!("new_invoice: {:?}", new_invoice);
        let mut invoice_builder = InvoiceBuilder::new(Currency::from_str(&new_invoice.currency)?)
            .amount(Some(new_invoice.amount));
        if let Some(description) = new_invoice.description {
            invoice_builder = invoice_builder.description(&description);
        };
        if let Some(payment_hash) = new_invoice.payment_hash {
            let vec = hex::decode(payment_hash)?;
            let payment_hash: [u8; 32] = vec_to_u8_32(vec).unwrap();
            invoice_builder = invoice_builder.payment_hash(payment_hash);
        };
        if let Some(payment_preimage) = new_invoice.payment_preimage {
            let vec = hex::decode(payment_preimage)?;
            let payment_preimage: [u8; 32] = vec_to_u8_32(vec).unwrap();
            invoice_builder = invoice_builder.payment_preimage(payment_preimage);
        };
        if let Some(expiry) = new_invoice.expiry {
            let duration: Duration = Duration::from_secs(expiry);
            invoice_builder = invoice_builder.expiry_time(duration);
        };

        let invoice = invoice_builder.build();
        if let Ok(invoice) = &invoice {
            self.invoices_db.insert_invoice(invoice.clone())?;
        }
        eprintln!("invoice: {:?}", invoice);
        invoice
    }

    async fn parse_invoice(
        &mut self,
        invoice: String,
        response: Option<mpsc::Sender<crate::Result<CkbInvoice>>>,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }
}
