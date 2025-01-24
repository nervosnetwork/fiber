use ractor::ActorRef;
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
    fn get_invoice_channels(&self, id: &Hash256) -> Vec<Hash256>;
    // This function is used to add a channel to the list of channels that were
    // used to pay an invoice.
    fn add_invoice_channel(
        &self,
        id: &Hash256,
        channel: &Hash256,
    ) -> Result<Vec<Hash256>, InvoiceError>;
}

#[derive(Error, Debug)]
pub(crate) enum SettleInvoiceError {
    #[error("Invoice not found")]
    InvoiceNotFound,
    #[error("Hash algorithm missing")]
    HashAlgorithmMissing,
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
        .get_invoice(&payment_hash)
        .ok_or(SettleInvoiceError::InvoiceNotFound)?;

    match invoice.hash_algorithm() {
        Some(hash_algorithm) => {
            let hash = hash_algorithm.hash(payment_preimage);
            if hash.as_slice() != payment_hash.as_ref() {
                return Err(SettleInvoiceError::HashMismatch);
            }
        }
        None => {
            return Err(SettleInvoiceError::HashAlgorithmMissing);
        }
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
    if let Some(network_actor) = network_actor {
        match store.get_invoice_status(payment_hash) {
            Some(CkbInvoiceStatus::Received) => {
                let channels = store.get_invoice_channels(payment_hash);
                for channel_id in channels {
                    let _ = network_actor.send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                            channel_id,
                            command: ChannelCommand::SettleHeldTlc(*payment_hash),
                        }),
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(())
}
