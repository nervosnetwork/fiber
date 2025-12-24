use crate::{
    fiber::{
        channel::{ChannelActorStateStore, RemoveTlcCommand, TLCId, TlcInfo},
        types::{Hash256, RemoveTlcFulfill, RemoveTlcReason, TlcErr, TlcErrPacket, TlcErrorCode},
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore, PreimageStore},
};

pub struct SettleTlcSetCommand<'s, S> {
    payment_hash: Hash256,
    tlcs: Vec<TlcSettlementContext>,
    store: &'s S,
}

#[derive(Debug)]
pub struct TlcSettlementContext {
    channel_id: Hash256,
    id: u64,
    amount: u128,
    total_amount: Option<u128>,
    shared_secret: [u8; 32],
}

impl TlcSettlementContext {
    pub fn new(tlc_info: &TlcInfo, channel_id: Hash256) -> Self {
        Self {
            channel_id,
            id: tlc_info.id(),
            amount: tlc_info.amount,
            total_amount: tlc_info.total_amount,
            shared_secret: tlc_info.shared_secret,
        }
    }

    fn total_amount_or_amount(&self) -> u128 {
        self.total_amount.unwrap_or(self.amount)
    }

    fn into_remove_tlc_fail_settlement(self, error_code: TlcErrorCode) -> TlcSettlement {
        TlcSettlement::new(
            self.channel_id,
            RemoveTlcCommand {
                id: self.id,
                reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                    TlcErr::new(error_code),
                    &self.shared_secret,
                )),
            },
        )
    }

    fn into_remove_tlc_fulfill_settlement(self, payment_preimage: Hash256) -> TlcSettlement {
        TlcSettlement::new(
            self.channel_id,
            RemoveTlcCommand {
                id: self.id,
                reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
            },
        )
    }
}

impl<'s, S> SettleTlcSetCommand<'s, S>
where
    S: PreimageStore + InvoiceStore + ChannelActorStateStore,
{
    pub fn new(payment_hash: Hash256, channel_tlc_ids: Vec<(Hash256, u64)>, store: &'s S) -> Self {
        let tlcs: Vec<_> = channel_tlc_ids
            .into_iter()
            .filter_map(|(channel_id, tlc_id)| {
                let state = store.get_channel_actor_state(&channel_id)?;
                let tlc_id = TLCId::Received(tlc_id);
                state
                    .get_received_tlc(tlc_id)
                    .map(|tlc_info| TlcSettlementContext::new(tlc_info, channel_id))
            })
            .collect();
        Self {
            payment_hash,
            tlcs,
            store,
        }
    }

    pub fn run(mut self) -> Vec<TlcSettlement> {
        let (Some(invoice), Some(invoice_status)) = (
            self.store.get_invoice(&self.payment_hash),
            self.store.get_invoice_status(&self.payment_hash),
        ) else {
            // TLC without invoice should not be added as hold TLC, reject them as invoice
            // canceled.
            return self.reject_all(TlcErrorCode::InvoiceCancelled);
        };

        if let Err(error_code) = self.verify(&invoice, &invoice_status) {
            return self.reject_all(error_code);
        }

        let mut rejected = self.leave_just_fulfilled_tlcs(&invoice);
        if self.tlcs.is_empty() {
            return rejected;
        }

        // Now we are sure the invoice is fulfilled, and `self.tlcs` is ready to be settled.
        // Update invoice status to Received
        self.mark_invoice_as_received_if_still_open(&invoice_status);

        let mut settlements = self.try_settle_all();
        settlements.append(&mut rejected);
        settlements
    }

    /// Leave just fulfilled tlcs, and reject the rest.
    ///
    /// Return settlements for rejected tlcs.
    ///
    /// When the invoice is not fulfilled:
    /// - If invoice allows mpp, clear tlcs and return empty settlements,
    /// - Otherwise, reject all tlcs and return settlements for rejected tlcs.
    ///
    /// When this function returns, and `self.tlcs` is not empty, it means the
    /// invoice is now fulfilled, and `self.tlcs` is ready to be settled.
    fn leave_just_fulfilled_tlcs(&mut self, invoice: &CkbInvoice) -> Vec<TlcSettlement> {
        if invoice.allow_mpp() {
            self.leave_just_fulfilled_tlcs_for_mpp_invoice(invoice)
        } else {
            self.leave_just_fulfilled_tlcs_for_non_mpp_invoice(invoice)
        }
    }

    fn leave_just_fulfilled_tlcs_for_mpp_invoice(
        &mut self,
        invoice: &CkbInvoice,
    ) -> Vec<TlcSettlement> {
        let Some(first_tlc) = self.tlcs.first() else {
            return Vec::new();
        };

        let total_amount = first_tlc.total_amount_or_amount();
        if total_amount < invoice.amount.unwrap_or_default() {
            return self.reject_all(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        }

        let mut accumulated_amount = 0;
        // Remove overpaid TLCs
        let mut retain_len: usize = 0;
        for tlc in self.tlcs.iter() {
            if accumulated_amount < total_amount {
                accumulated_amount = accumulated_amount.saturating_add(tlc.amount);
                retain_len += 1;
            }
        }

        // If not fulfilled, clear tlcs and return empty settlements.
        if accumulated_amount < total_amount {
            self.tlcs.clear();
            Vec::new()
        } else {
            let overpaid_tlcs = self.tlcs.split_off(retain_len);
            self.reject_tlcs(overpaid_tlcs, TlcErrorCode::HoldTlcTimeout)
        }
    }

    fn leave_just_fulfilled_tlcs_for_non_mpp_invoice(
        &mut self,
        invoice: &CkbInvoice,
    ) -> Vec<TlcSettlement> {
        let required_amount = invoice.amount.unwrap_or_default();

        let Some(index) = self
            .tlcs
            .iter()
            .position(|tlc| tlc.amount >= required_amount)
        else {
            // No one fulfilled, reject all
            return self.reject_all(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        };

        let mut rejected_tlcs = std::mem::take(&mut self.tlcs);
        self.tlcs.push(rejected_tlcs.swap_remove(index));
        self.reject_tlcs(rejected_tlcs, TlcErrorCode::HoldTlcTimeout)
    }

    /// Verify TLCs against the invoice.
    ///
    /// Returns `Err` to reject all tlcs with the error code.
    fn verify(
        &self,
        invoice: &CkbInvoice,
        invoice_status: &CkbInvoiceStatus,
    ) -> Result<(), TlcErrorCode> {
        self.verify_invoice_status(invoice_status)?;
        self.verify_mpp_tlcs_have_consistent_total_amount(invoice)?;
        Ok(())
    }

    fn verify_invoice_status(&self, invoice_status: &CkbInvoiceStatus) -> Result<(), TlcErrorCode> {
        match invoice_status {
            CkbInvoiceStatus::Open | CkbInvoiceStatus::Received => Ok(()),
            CkbInvoiceStatus::Expired => Err(TlcErrorCode::InvoiceExpired),
            CkbInvoiceStatus::Cancelled => Err(TlcErrorCode::InvoiceCancelled),
            // When invoice is paid, TLCs will eventually timeout, so we reject them now with the same reason.
            CkbInvoiceStatus::Paid => Err(TlcErrorCode::HoldTlcTimeout),
        }
    }

    fn verify_mpp_tlcs_have_consistent_total_amount(
        &self,
        invoice: &CkbInvoice,
    ) -> Result<(), TlcErrorCode> {
        if invoice.allow_mpp()
            && self.tlcs.len() > 1
            && !self
                .tlcs
                .windows(2)
                .all(|w| w[0].total_amount == w[1].total_amount)
        {
            tracing::error!("TLCs have inconsistent total_amount");
            return Err(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        }
        Ok(())
    }

    fn try_settle_all(self) -> Vec<TlcSettlement> {
        if let Some(payment_preimage) = self.store.get_preimage(&self.payment_hash) {
            self.tlcs
                .into_iter()
                .map(|tlc| tlc.into_remove_tlc_fulfill_settlement(payment_preimage))
                .collect()
        } else {
            // Skip this time and retry later when the preimage is available.
            self.skip_all()
        }
    }

    fn reject_tlcs(
        &self,
        tlcs: Vec<TlcSettlementContext>,
        error_code: TlcErrorCode,
    ) -> Vec<TlcSettlement> {
        tracing::error!(
            "Remove TLCs for payment hash {} because of error {:?}",
            self.payment_hash,
            error_code,
        );
        tracing::debug!(
            "Removed TLCs for payment hash {} because of error {:?}: {:?}",
            self.payment_hash,
            error_code,
            tlcs,
        );
        tlcs.into_iter()
            .map(|tlc| tlc.into_remove_tlc_fail_settlement(error_code))
            .collect()
    }

    fn reject_all(&mut self, error_code: TlcErrorCode) -> Vec<TlcSettlement> {
        let tlcs = std::mem::take(&mut self.tlcs);
        self.reject_tlcs(tlcs, error_code)
    }

    fn skip_all(self) -> Vec<TlcSettlement> {
        Vec::new()
    }

    fn mark_invoice_as_received_if_still_open(&self, invoice_status: &CkbInvoiceStatus) {
        if *invoice_status == CkbInvoiceStatus::Open {
            self.store
                .update_invoice_status(&self.payment_hash, CkbInvoiceStatus::Received)
                .expect("update invoice status failed");
        }
    }
}

pub struct TlcSettlement {
    channel_id: Hash256,
    remove_tlc_command: RemoveTlcCommand,
}

impl TlcSettlement {
    pub fn new(channel_id: Hash256, remove_tlc_command: RemoveTlcCommand) -> Self {
        Self {
            channel_id,
            remove_tlc_command,
        }
    }

    pub fn channel_id(&self) -> Hash256 {
        self.channel_id
    }

    pub fn tlc_id(&self) -> u64 {
        self.remove_tlc_command.id
    }

    pub fn remove_tlc_command(&self) -> &RemoveTlcCommand {
        &self.remove_tlc_command
    }
}
