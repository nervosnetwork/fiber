use super::{InvoiceCommand, InvoicesDb, NewInvoiceParams};
use anyhow::Result;
use tokio::{select, sync::mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub async fn start_invoice(
    command_receiver: mpsc::Receiver<InvoiceCommand>,
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
    command_receiver: mpsc::Receiver<InvoiceCommand>,
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
                            let command_name = command.name();
                            log::info!("Process cch command {}", command_name);

                            match self.process_command(command).await {
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

    async fn process_command(&mut self, command: InvoiceCommand) -> Result<()> {
        log::debug!("InvoiceCommand received: {:?}", command);
        match command {
            InvoiceCommand::NewInvoice(params) => self.new_invoice(params).await,
            InvoiceCommand::ParseInvoice(params) => self.parse_invoice(params).await,
        }
    }

    async fn new_invoice(&mut self, new_invoice: NewInvoiceParams) -> Result<()> {
        Ok(())
    }

    async fn parse_invoice(&mut self, invoice: String) -> Result<()> {
        Ok(())
    }
}
