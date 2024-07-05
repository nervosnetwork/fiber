use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use hex::ToHex;
use lightning_invoice::Bolt11Invoice;
use tokio::{select, sync::mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::{CchCommand, CchConfig, CchError, CchOrderStatus, CchOrdersDb, SendBTC, SendBTCOrder};

pub async fn start_cch(
    config: CchConfig,
    command_receiver: mpsc::Receiver<CchCommand>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let service = CchService {
        config,
        command_receiver,
        token,
        orders_db: Default::default(),
    };
    tracker.spawn(async move {
        service.run().await;
    });
}
struct CchService {
    config: CchConfig,
    token: CancellationToken,
    command_receiver: mpsc::Receiver<CchCommand>,
    orders_db: CchOrdersDb,
}

impl CchService {
    pub async fn run(mut self) {
        loop {
            select! {
                _ = self.token.cancelled() => {
                    crate::debug!("Cancellation received, shutting down cch service");
                    break;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        None => {
                            crate::debug!("Command receiver completed, shutting down tentacle service");
                            break;
                        }
                        Some(command) => {
                            let command_name = command.name();
                            crate::info!("Process cch command {}", command_name);

                            match self.process_command(command).await {
                                Ok(_) => {}
                                Err(err) => {
                                    crate::error!("Error processing command {}: {:?}", command_name, err);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn process_command(&mut self, command: CchCommand) -> Result<()> {
        crate::debug!("CchCommand received: {:?}", command);
        match command {
            CchCommand::SendBTC(send_btc) => self.send_btc(send_btc).await,
        }
    }

    async fn send_btc(&mut self, send_btc: SendBTC) -> Result<()> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        crate::debug!("BTC invoice: {:?}", invoice);

        let expiry = invoice
            .expires_at()
            .and_then(|expired_at| expired_at.checked_sub(duration_since_epoch))
            .map(|duration| duration.as_secs())
            .or_else(|| {
                self.config
                    .allow_expired_btc_invoice
                    .then_some(self.config.order_expiry)
            })
            .ok_or(CchError::BTCInvoiceExpired)?;

        let amount_msat = invoice
            .amount_milli_satoshis()
            .ok_or(CchError::BTCInvoiceMissingAmount)?;

        crate::debug!("SendBTC expiry: {:?}", expiry);
        let (ratio_ckb_shannons, ratio_btc_msat) =
            match (self.config.ratio_ckb_shannons, self.config.ratio_btc_msat) {
                (Some(ratio_ckb_shannons), Some(ratio_btc_msat)) => {
                    (ratio_ckb_shannons, ratio_btc_msat)
                }
                _ => return Err(CchError::CKBAssetNotAllowed.into()),
            };
        let order_value = ((ratio_ckb_shannons as u128) * (amount_msat as u128)
            / (ratio_btc_msat as u128)) as u64;
        let fee = order_value * self.config.fee_rate_per_million_shannons / 1_000_000
            + self.config.base_fee_shannons;

        let order = SendBTCOrder {
            timestamp: duration_since_epoch.as_secs(),
            expiry,
            ckb_final_tlc_expiry: self.config.ckb_final_tlc_expiry,
            btc_pay_req: send_btc.btc_pay_req,
            payment_hash: invoice.payment_hash().encode_hex(),
            amount_shannons: order_value + fee,
            fulfilled_amount_shannons: 0u64,
            status: CchOrderStatus::Pending,
        };

        // TODO: Return it as the RPC response
        crate::info!("SendBTCOrder: {}", serde_json::to_string(&order)?);
        self.orders_db.insert_send_btc_order(order).await?;

        Ok(())
    }
}
