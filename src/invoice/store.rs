use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{CkbInvoiceStatus, InvoiceError};
use crate::{
    fiber::{
        channel::{ChannelCommand, ChannelCommandWithId},
        types::Hash256,
        NetworkActorCommand, NetworkActorMessage,
    },
    invoice::CkbInvoice,
};

pub trait InvoiceStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice>;
    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError>;
    fn get_invoice_preimage(&self, id: &Hash256) -> Option<Hash256>;
    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: CkbInvoiceStatus,
    ) -> Result<(), InvoiceError>;
    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus>;
    fn insert_payment_preimage(
        &self,
        payment_hash: Hash256,
        preimage: Hash256,
    ) -> Result<(), InvoiceError>;
    // A payment to an invoice is made by sending a TLC over some channels
    // (possibly multiple when atomic multi-path payment support is out).
    // This function returns all the channels that were used to pay an invoice.
    fn get_invoice_channel_info(&self, payment_hash: &Hash256) -> Vec<InvoiceChannelInfo>;
    // This function is used to add a channel (with the amount paid through this channel)
    // to the list of channels that were used to pay an invoice.
    fn add_invoice_channel_info(
        &self,
        payment_hash: &Hash256,
        invoice_channel_info: InvoiceChannelInfo,
    ) -> Result<Vec<InvoiceChannelInfo>, InvoiceError>;
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct InvoiceChannelInfo {
    pub channel_id: Hash256,
    pub amount: u128,
}

impl InvoiceChannelInfo {
    pub fn new(channel_id: Hash256, amount: u128) -> Self {
        Self { channel_id, amount }
    }
}

#[derive(Error, Debug)]
pub enum SettleInvoiceError {
    #[error("Invoice not found")]
    InvoiceNotFound,
    #[error("Hash mismatch")]
    HashMismatch,
    #[error("Internal error: {0}")]
    InternalError(String),
}

// This function is used to settle an invoice. It is extracted from the `InvoiceRpcServerImpl` struct
// to share it with test functions.
pub(crate) fn settle_invoice<S: InvoiceStore>(
    store: &S,
    network_actor: Option<&ActorRef<NetworkActorMessage>>,
    payment_hash: &Hash256,
    payment_preimage: &Hash256,
) -> Result<(), SettleInvoiceError> {
    let invoice = store
        .get_invoice(payment_hash)
        .ok_or(SettleInvoiceError::InvoiceNotFound)?;

    let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
    let hash = hash_algorithm.hash(payment_preimage);
    if hash.as_slice() != payment_hash.as_ref() {
        return Err(SettleInvoiceError::HashMismatch);
    }

    match store.insert_payment_preimage(*payment_hash, *payment_preimage) {
        Ok(_) => {}
        Err(e) => {
            return Err(SettleInvoiceError::InternalError(format!(
                "Failed to save payment preimage: {:?}",
                e
            )));
        }
    }

    // We will send network actor a message to settle the invoice immediately if possible.
    if let (Some(CkbInvoiceStatus::Received), Some(network_actor)) =
        (store.get_invoice_status(payment_hash), network_actor)
    {
        let channels = store.get_invoice_channel_info(payment_hash);
        let total_amount: u128 = channels.iter().map(|c| c.amount).sum();
        match invoice.amount() {
            Some(amount) if total_amount < amount => {
                return Ok(());
            }
            _ => {
                // Only settle the invoice if the client has paid the full amount.
                for channel in channels {
                    let _ = network_actor.send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                            channel_id: channel.channel_id,
                            command: ChannelCommand::SettleHeldTlc(*payment_hash),
                        }),
                    ));
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn add_invoice<S: InvoiceStore>(
    store: &S,
    invoice: CkbInvoice,
    preimage: Option<Hash256>,
) -> Result<(), InvoiceError> {
    let hash = invoice.payment_hash();
    if store.get_invoice(hash).is_some() {
        return Err(InvoiceError::InvoiceAlreadyExists);
    }
    store.insert_invoice(invoice, preimage)
}
